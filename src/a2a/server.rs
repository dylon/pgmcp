//! Axum routes for the A2A surface.

#![allow(dead_code)]

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use futures::stream::Stream;
use uuid::Uuid;

use crate::api::ApiState;

use super::handlers;
use super::skills::build_agent_card;
use super::sse as a2a_sse;
use super::types::{AgentCard, JsonRpcRequest, JsonRpcResponse};

/// Build the A2A router. Mounted by `cli/daemon.rs` alongside the existing
/// REST API routes.
pub fn a2a_router() -> Router<ApiState> {
    Router::new()
        .route("/.well-known/agent.json", get(get_agent_card))
        .route("/a2a/jsonrpc", post(handle_jsonrpc))
        .route("/a2a/sse/{task_id}", get(stream_task_events))
        .route("/a2a/agents", get(list_agents).post(register_agent))
        .route("/a2a/agents/active", get(active_agents))
        .route("/a2a/messages", post(post_message).get(get_messages))
}

#[derive(serde::Deserialize)]
struct ActiveAgentsQuery {
    project: Option<String>,
}

#[derive(serde::Deserialize)]
struct SendMessageBody {
    from_agent: Option<String>,
    from_session: Option<String>,
    to_session: Option<String>,
    to_project_id: Option<i32>,
    to_agent: Option<String>,
    kind: Option<String>,
    subject: Option<String>,
    body: String,
    reply_to: Option<i64>,
    expires_minutes: Option<i64>,
}

/// `POST /a2a/messages` — enqueue a mailbox message (REST twin of `a2a_send_message`).
async fn post_message(
    State(state): State<ApiState>,
    Json(b): Json<SendMessageBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let pool = state
        .db
        .pool()
        .ok_or((StatusCode::INTERNAL_SERVER_ERROR, "no pool".into()))?;
    if b.to_session.is_none() && b.to_project_id.is_none() && b.to_agent.is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            "specify at least one of to_session / to_project_id / to_agent".into(),
        ));
    }
    let kind = b.kind.as_deref().unwrap_or("message");
    if crate::a2a::mailbox::MessageKind::parse(kind).is_none() {
        return Err((StatusCode::BAD_REQUEST, format!("invalid kind '{kind}'")));
    }
    let expires_at = b
        .expires_minutes
        .map(|m| chrono::Utc::now() + chrono::Duration::minutes(m));
    let msg = crate::a2a::mailbox_store::NewMessage {
        from_agent: b.from_agent.as_deref().unwrap_or("unknown"),
        from_session: b.from_session.as_deref(),
        to_session: b.to_session.as_deref(),
        to_project_id: b.to_project_id,
        to_agent: b.to_agent.as_deref(),
        kind,
        subject: b.subject.as_deref(),
        body: &b.body,
        reply_to: b.reply_to,
        expires_at,
    };
    let id = crate::a2a::mailbox_store::send(pool, &msg)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({ "message_id": id })))
}

#[derive(serde::Deserialize)]
struct InboxQuery {
    session: Option<String>,
    project_id: Option<i32>,
    agent: Option<String>,
    #[serde(default)]
    unread_only: bool,
}

/// `GET /a2a/messages?session=|project_id=|agent=` — inbox (REST twin of `a2a_inbox`).
async fn get_messages(
    State(state): State<ApiState>,
    axum::extract::Query(q): axum::extract::Query<InboxQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let pool = state
        .db
        .pool()
        .ok_or((StatusCode::INTERNAL_SERVER_ERROR, "no pool".into()))?;
    if q.session.is_none() && q.project_id.is_none() && q.agent.is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            "specify at least one of session / project_id / agent".into(),
        ));
    }
    let rows = crate::a2a::mailbox_store::inbox(
        pool,
        q.session.as_deref(),
        q.project_id,
        q.agent.as_deref(),
        q.unread_only,
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(
        serde_json::json!({ "count": rows.len(), "messages": rows }),
    ))
}

/// `GET /a2a/agents/active?project=<name>` — live agent instances and the
/// project each is on (the A2A active-agents-by-project discovery view; the
/// REST twin of the `a2a_active_agents` MCP tool).
async fn active_agents(
    State(state): State<ApiState>,
    axum::extract::Query(q): axum::extract::Query<ActiveAgentsQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let pool = state
        .db
        .pool()
        .ok_or((StatusCode::INTERNAL_SERVER_ERROR, "no pool".into()))?;
    let rows = crate::db::queries::active_agents_by_project(pool, q.project.as_deref())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let agents: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "client_name": r.client_name,
                "mcp_session_id": r.mcp_session_id,
                "pid": r.pid,
                "cwd": r.cwd,
                "project": r.project,
                "project_id": r.project_id,
                "alive": r.alive,
                "last_seen": r.last_seen,
                "recommended_role": r.recommended_role,
                "specialty": r.specialty,
            })
        })
        .collect();
    let count = agents.len();
    Ok(Json(
        serde_json::json!({ "active_agents": agents, "count": count }),
    ))
}

async fn get_agent_card(State(state): State<ApiState>) -> Json<AgentCard> {
    let base = derive_base_url(&state);
    Json(build_agent_card(&base))
}

fn derive_base_url(state: &ApiState) -> String {
    let cfg = state.config.load();
    let port = cfg.mcp.port;
    format!("http://{}:{}", cfg.mcp.host, port)
}

async fn handle_jsonrpc(
    State(state): State<ApiState>,
    Json(req): Json<JsonRpcRequest>,
) -> Json<JsonRpcResponse> {
    Json(handlers::dispatch(&state, req).await)
}

async fn list_agents(
    State(state): State<ApiState>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let pool = state
        .db
        .pool()
        .ok_or((StatusCode::INTERNAL_SERVER_ERROR, "no pool".into()))?;
    let rows: Vec<(String, String, String, String, serde_json::Value, serde_json::Value)> =
        sqlx::query_as::<_, (String, String, String, String, serde_json::Value, serde_json::Value)>(
            "SELECT name, version, COALESCE(description, ''), url, capabilities, skills FROM a2a_agents",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let agents: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|(name, version, description, url, capabilities, skills)| {
            serde_json::json!({
                "name": name, "version": version, "description": description,
                "url": url, "capabilities": capabilities, "skills": skills,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "agents": agents })))
}

async fn register_agent(
    State(state): State<ApiState>,
    Json(payload): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let pool = state
        .db
        .pool()
        .ok_or((StatusCode::INTERNAL_SERVER_ERROR, "no pool".into()))?;
    let name = payload
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "name required".into()))?
        .to_string();
    let url = payload
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "url required".into()))?
        .to_string();
    let version = payload
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let description = payload
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let capabilities = payload
        .get("capabilities")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let skills = payload
        .get("skills")
        .cloned()
        .unwrap_or_else(|| serde_json::json!([]));
    // Optional RecursiveMAS-inspired metadata for routing.
    let specialty: Vec<String> = payload
        .get("specialty")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let recommended_role: Option<String> = payload
        .get("recommendedRole")
        .or_else(|| payload.get("recommended_role"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    sqlx::query(
        "INSERT INTO a2a_agents
            (name, version, description, url, capabilities, skills,
             specialty, recommended_role)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (name) DO UPDATE SET
             version = EXCLUDED.version,
             description = EXCLUDED.description,
             url = EXCLUDED.url,
             capabilities = EXCLUDED.capabilities,
             skills = EXCLUDED.skills,
             specialty = EXCLUDED.specialty,
             recommended_role = EXCLUDED.recommended_role,
             last_seen_at = NOW()",
    )
    .bind(&name)
    .bind(&version)
    .bind(&description)
    .bind(&url)
    .bind(&capabilities)
    .bind(&skills)
    .bind(&specialty)
    .bind(&recommended_role)
    .execute(pool)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({"registered": name})))
}

async fn stream_task_events(
    State(state): State<ApiState>,
    Path(task_id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, axum::Error>>>, (StatusCode, String)> {
    let task_uuid = Uuid::parse_str(&task_id)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("bad task_id: {}", e)))?;
    let pool = state
        .db
        .pool()
        .ok_or((StatusCode::INTERNAL_SERVER_ERROR, "no pool".into()))?
        .clone();
    let stream = a2a_sse::task_event_stream(Arc::new(pool), task_uuid).await;
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}
