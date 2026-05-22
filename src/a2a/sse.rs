//! SSE bridge: serves `/a2a/sse/{task_id}` by streaming `a2a_events` rows.
//!
//! Two streams are merged: (1) a polling loop that reads new rows from
//! `a2a_events` since the last seen `sequence`; (2) once polled, the stream
//! ends when a `final` event is observed. A Postgres LISTEN/NOTIFY channel
//! per-task would be ideal but introduces extra connection complexity; the
//! polling approach (200ms cadence) is adequate for typical Task durations
//! and avoids a separate listener pool.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use axum::response::sse::Event as SseEvent;
use futures::stream::{self, Stream};
use sqlx::PgPool;
use tokio::time::sleep;
use tracing::warn;
use uuid::Uuid;

const POLL_INTERVAL: Duration = Duration::from_millis(200);
const STREAM_TIMEOUT: Duration = Duration::from_secs(300);

/// Notify-channel name for a task's events. Reserved for future
/// LISTEN/NOTIFY integration; currently the polling loop in
/// `task_event_stream` provides the same fanout without LISTEN holding
/// a connection open.
pub fn channel_name(task_id: Uuid) -> String {
    format!("a2a_task_{}", task_id.simple())
}

/// Emit a LISTEN/NOTIFY notification for a task event. Best-effort.
pub async fn notify_task(
    pool: &PgPool,
    task_id: Uuid,
    kind: &str,
    payload: &serde_json::Value,
) -> Result<(), sqlx::Error> {
    let ch = channel_name(task_id);
    let payload_str = serde_json::json!({"kind": kind, "payload": payload}).to_string();
    sqlx::query("SELECT pg_notify($1, $2)")
        .bind(&ch)
        .bind(&payload_str)
        .execute(pool)
        .await?;
    Ok(())
}

/// Build the SSE stream for a Task. Polls `a2a_events` every 200ms,
/// streams new rows, ends when a `final` event is seen or after
/// `STREAM_TIMEOUT`.
pub async fn task_event_stream(
    pool: Arc<PgPool>,
    task_id: Uuid,
) -> impl Stream<Item = Result<SseEvent, axum::Error>> {
    let pool = pool.clone();
    let started = std::time::Instant::now();
    stream::unfold(
        (pool, task_id, -1i32, started, false),
        |(pool, task_id, last_seq, started, done)| async move {
            if done || started.elapsed() > STREAM_TIMEOUT {
                return None;
            }
            // Drain any pending rows.
            let rows = match sqlx::query_as::<_, (i32, String, serde_json::Value)>(
                "SELECT sequence, kind, payload FROM a2a_events
                 WHERE task_id = $1 AND sequence > $2
                 ORDER BY sequence",
            )
            .bind(task_id)
            .bind(last_seq)
            .fetch_all(&*pool)
            .await
            {
                Ok(r) => r,
                Err(e) => {
                    warn!(error = %e, "a2a sse poll failed");
                    return None;
                }
            };
            if rows.is_empty() {
                sleep(POLL_INTERVAL).await;
                return Some((
                    Ok(SseEvent::default().comment("heartbeat")),
                    (pool, task_id, last_seq, started, false),
                ));
            }
            // We can only return one Event per yield; build a batch JSON.
            let last = rows.last().expect("non-empty").0;
            let any_final = rows.iter().any(|(_, k, _)| k == "final");
            let payload = serde_json::json!({
                "events": rows.iter().map(|(seq, kind, payload)| serde_json::json!({
                    "sequence": seq, "kind": kind, "payload": payload
                })).collect::<Vec<_>>(),
            });
            let evt = SseEvent::default().event("a2a").data(payload.to_string());
            Some((Ok(evt), (pool, task_id, last, started, any_final)))
        },
    )
}
