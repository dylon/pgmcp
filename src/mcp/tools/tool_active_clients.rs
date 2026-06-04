//! `tool_active_clients` — MCP tool body: live MCP clients and the project each
//! is working on (PID · cwd · liveness · idle), grouped by project.

#![allow(unused_imports)]

use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::json;
use tracing::debug;

use crate::context::SystemContext;
use crate::mcp::server::*;

pub async fn tool_active_clients(
    ctx: &SystemContext,
    params: ActiveClientsParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    debug!(tool = "active_clients", "MCP tool invoked");

    let Some(pool) = ctx.db().pool() else {
        return Err(McpError::internal_error(
            "database pool unavailable".to_string(),
            None,
        ));
    };

    let rows =
        crate::db::queries::active_clients(pool, params.project.as_deref(), params.include_exited)
            .await
            .map_err(|e| {
                McpError::internal_error(format!("active_clients query failed: {e}"), None)
            })?;

    let total = rows.len();

    // Group by project for a "who is working on what" view. The query orders by
    // project_id then recency, so rows for one project are contiguous.
    let mut groups: Vec<(Option<String>, Vec<serde_json::Value>)> = Vec::new();
    for r in &rows {
        let entry = json!({
            "client_name": r.client_name,
            "mcp_session_id": r.mcp_session_id,
            "client_version": r.client_version,
            "pid": r.pid,
            "cwd": r.cwd,
            "alive": r.alive,
            "idle_secs": r.idle_secs,
            "first_seen": r.first_seen,
            "last_seen": r.last_seen,
            "last_liveness_at": r.last_liveness_at,
        });
        match groups.last_mut() {
            Some((proj, list)) if *proj == r.project => list.push(entry),
            _ => groups.push((r.project.clone(), vec![entry])),
        }
    }
    let by_project: Vec<serde_json::Value> = groups
        .into_iter()
        .map(|(project, clients)| {
            json!({
                "project": project,
                "client_count": clients.len(),
                "clients": clients,
            })
        })
        .collect();

    let envelope = json!({
        "total": total,
        "by_project": by_project,
    });

    let body = serde_json::to_string_pretty(&envelope)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {e}"), None))?;

    debug!(
        tool = "active_clients",
        total,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(body)]))
}
