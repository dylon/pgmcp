use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::{Sink, SinkExt, StreamExt};
use serde::{Deserialize, Serialize};

use crate::db;
use crate::security;
use crate::state::{ConnectionPermit, FixedWindowRateLimiter, WebuiState};

const TOPICS: &[&str] = &[
    "tracker", "mandate", "cron", "task", "index", "client", "scanner", "control", "trace",
    "status",
];

#[derive(Debug, Default, Deserialize)]
pub struct WsQuery {
    #[serde(default)]
    since: Option<i64>,
    #[serde(default)]
    topics: Option<String>,
    #[serde(default)]
    token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ClientFrame {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    since: Option<i64>,
    #[serde(default)]
    topics: Option<Vec<String>>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerFrame<'a> {
    Welcome { server_seq: i64 },
    Heartbeat { server_seq: i64 },
    Event { event: &'a db::RealtimeEventRow },
    Resync { reason: &'a str, server_seq: i64 },
}

pub async fn handler(
    ws: WebSocketUpgrade,
    State(state): State<WebuiState>,
    Query(query): Query<WsQuery>,
    headers: HeaderMap,
) -> Response {
    if !security::origin_allowed(&headers, &state.options().allowed_origins) {
        return (StatusCode::FORBIDDEN, "webui origin denied").into_response();
    }
    if !security::token_authorized(
        &headers,
        query.token.as_deref(),
        state.options().token.as_deref(),
    ) {
        return (StatusCode::UNAUTHORIZED, "webui token required").into_response();
    }
    if !state.handshake_allowed(Instant::now()) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "webui handshake rate exceeded",
        )
            .into_response();
    }
    let Some(permit) = state.try_acquire_connection() else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "webui connection limit reached",
        )
            .into_response();
    };
    let topics = match parse_topics(query.topics.as_deref()) {
        Ok(topics) => topics,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };
    let since = query.since.unwrap_or(0).max(0);
    ws.on_upgrade(move |socket| socket_loop(socket, state, since, topics, permit))
}

async fn socket_loop(
    socket: WebSocket,
    state: WebuiState,
    since: i64,
    topics: Vec<String>,
    _permit: ConnectionPermit,
) {
    let (mut sender, mut receiver) = socket.split();
    let mut after_seq = since.max(0);
    let mut topics = topics;
    let heartbeat_secs = state.options().heartbeat_secs.max(1);
    let replay_page = state.options().replay_page.clamp(1, 2_000);
    let mut rate_limiter = FixedWindowRateLimiter::new(
        state.options().max_msgs_per_sec,
        Duration::from_secs(1),
        Instant::now(),
    );
    let mut tick = tokio::time::interval(Duration::from_secs(heartbeat_secs));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let server_seq = db::current_seq(state.pool()).await.unwrap_or(0);
    if send_frame(&mut sender, &ServerFrame::Welcome { server_seq })
        .await
        .is_err()
    {
        return;
    }

    loop {
        tokio::select! {
            maybe_msg = receiver.next() => {
                match maybe_msg {
                    Some(Ok(message)) => {
                        if !rate_limiter.allow(Instant::now()) {
                            let server_seq = db::current_seq(state.pool()).await.unwrap_or(after_seq);
                            let _ = send_frame(&mut sender, &ServerFrame::Resync {
                                reason: "rate_limited",
                                server_seq,
                            }).await;
                            return;
                        }
                        match message {
                            Message::Text(text) => {
                                match handle_client_text(&text, &mut after_seq, &mut topics) {
                                    Ok(true) => {
                                        let server_seq = db::current_seq(state.pool()).await.unwrap_or(after_seq);
                                        if send_frame(&mut sender, &ServerFrame::Welcome { server_seq }).await.is_err() {
                                            return;
                                        }
                                    }
                                    Ok(false) => {}
                                    Err(reason) => {
                                        let server_seq = db::current_seq(state.pool()).await.unwrap_or(after_seq);
                                        if send_frame(&mut sender, &ServerFrame::Resync { reason, server_seq }).await.is_err() {
                                            return;
                                        }
                                    }
                                }
                            }
                            Message::Close(_) => return,
                            Message::Ping(payload) => {
                                if sender.send(Message::Pong(payload)).await.is_err() {
                                    return;
                                }
                            }
                            _ => {}
                        }
                    }
                    None => return,
                    Some(Err(e)) => {
                        tracing::debug!(error = %e, "webui websocket receive failed");
                        return;
                    }
                }
            }
            _ = tick.tick() => {
                match db::committed_events_after(state.pool(), after_seq, replay_page, &topics).await {
                    Ok(events) => {
                        if events.is_empty() {
                            let server_seq = db::current_seq(state.pool()).await.unwrap_or(after_seq);
                            if send_frame(&mut sender, &ServerFrame::Heartbeat { server_seq }).await.is_err() {
                                return;
                            }
                        } else {
                            for event in &events {
                                after_seq = after_seq.max(event.seq);
                                if send_frame(&mut sender, &ServerFrame::Event { event }).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "webui websocket replay query failed");
                        let server_seq = db::current_seq(state.pool()).await.unwrap_or(after_seq);
                        if send_frame(&mut sender, &ServerFrame::Resync { reason: "replay_query_failed", server_seq }).await.is_err() {
                            return;
                        }
                    }
                }
            }
        }
    }
}

fn handle_client_text(
    text: &str,
    after_seq: &mut i64,
    topics: &mut Vec<String>,
) -> Result<bool, &'static str> {
    if text.len() > 32 * 1024 {
        return Err("frame_too_large");
    }
    let frame: ClientFrame = serde_json::from_str(text).map_err(|_| "bad_json")?;
    match frame.kind.as_str() {
        "hello" => {
            *after_seq = frame.since.unwrap_or(*after_seq).max(0);
            if let Some(next_topics) = frame.topics {
                *topics = validate_topic_vec(next_topics).map_err(|_| "bad_topics")?;
            }
            Ok(true)
        }
        "ping" => Ok(false),
        _ => Err("unknown_frame"),
    }
}

async fn send_frame<S>(sender: &mut S, frame: &ServerFrame<'_>) -> Result<(), axum::Error>
where
    S: Sink<Message, Error = axum::Error> + Unpin,
{
    let text = serde_json::to_string(frame).unwrap_or_else(|_| {
        "{\"type\":\"resync\",\"reason\":\"serialize_failed\",\"server_seq\":0}".to_string()
    });
    sender.send(Message::Text(text.into())).await
}

fn parse_topics(raw: Option<&str>) -> Result<Vec<String>, &'static str> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    validate_topic_vec(
        raw.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .collect(),
    )
    .map_err(|_| "topics must be a comma-separated subset of the closed webui topic set")
}

fn validate_topic_vec(values: Vec<String>) -> Result<Vec<String>, ()> {
    let mut out = Vec::new();
    for value in values {
        let value = value.trim().to_ascii_lowercase();
        if !TOPICS.contains(&value.as_str()) {
            return Err(());
        }
        if !out.contains(&value) {
            out.push(value);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topics_are_closed_and_deduped() {
        assert_eq!(
            parse_topics(Some("tracker,cron,tracker")).unwrap(),
            vec!["tracker".to_string(), "cron".to_string()]
        );
        assert!(parse_topics(Some("tracker,unknown")).is_err());
    }

    #[test]
    fn hello_updates_cursor_and_topics_without_synthesizing_server_seq() {
        let mut after_seq = 42;
        let mut topics = vec!["tracker".to_string()];
        let should_ack = handle_client_text(
            r#"{"type":"hello","since":7,"topics":["cron","cron","task"]}"#,
            &mut after_seq,
            &mut topics,
        )
        .expect("hello frame should be valid");

        assert!(should_ack);
        assert_eq!(after_seq, 7);
        assert_eq!(topics, vec!["cron".to_string(), "task".to_string()]);
    }
}
