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
