//! `tool_a2a_active_agents` — MCP tool body: live agent instances grouped by
//! project (the A2A social discovery view), enriched with the advisory A2A
//! registry role/specialty. The returned `mcp_session_id` is the precise
//! instance handle for addressing a message with `a2a_send_message`.

#![allow(unused_imports)]

use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::json;
use tracing::debug;

use crate::context::SystemContext;
use crate::mcp::server::*;

pub async fn tool_a2a_active_agents(
    ctx: &SystemContext,
    params: A2aActiveAgentsParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    debug!(tool = "a2a_active_agents", "MCP tool invoked");

    let Some(pool) = ctx.db().pool() else {
        return Err(McpError::internal_error(
            "database pool unavailable".to_string(),
            None,
        ));
    };

    let rows = crate::db::queries::active_agents_by_project(pool, params.project.as_deref())
        .await
        .map_err(|e| McpError::internal_error(format!("active_agents query failed: {e}"), None))?;
    let total = rows.len();

    // Group by project (rows ordered by project_id).
    let mut groups: Vec<(Option<String>, Vec<serde_json::Value>)> = Vec::new();
    for r in &rows {
        let agent = json!({
            "client_name": r.client_name,
            "mcp_session_id": r.mcp_session_id,
            "pid": r.pid,
            "cwd": r.cwd,
            "alive": r.alive,
            "last_seen": r.last_seen,
            "recommended_role": r.recommended_role,
            "specialty": r.specialty,
        });
        match groups.last_mut() {
            Some((proj, list)) if *proj == r.project => list.push(agent),
            _ => groups.push((r.project.clone(), vec![agent])),
        }
    }
    let by_project: Vec<serde_json::Value> = groups
        .into_iter()
        .map(|(project, agents)| {
            json!({
                "project": project,
                "agent_count": agents.len(),
                "agents": agents,
            })
        })
        .collect();

    let envelope = json!({ "total": total, "by_project": by_project });
    let body = serde_json::to_string_pretty(&envelope)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {e}"), None))?;

    debug!(
        tool = "a2a_active_agents",
        total,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(body)]))
}
