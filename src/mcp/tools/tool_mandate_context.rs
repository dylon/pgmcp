//! `tool_mandate_context` — effective workspace/project mandates,
//! optionally augmented with session-scoped + durable mandates.

use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::json;
use tracing::{debug, error, info};
use uuid::Uuid;

use crate::context::SystemContext;
use crate::mandates::{resolve_effective_mandates, resolve_project_for_mandates};
use crate::mcp::server::MandateContextParams;
use crate::sessions;

pub async fn tool_mandate_context(
    ctx: &SystemContext,
    params: MandateContextParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    debug!(
        tool = "mandate_context",
        project = ?params.project,
        cwd = ?params.cwd,
        session = ?params.session_id,
        "MCP tool invoked"
    );

    let project = resolve_project_for_mandates(
        ctx.db().as_ref(),
        params.project.as_deref(),
        params.cwd.as_deref(),
    )
    .await
    .map_err(|e| {
        error!(tool = "mandate_context", error = %e, "MCP tool failed");
        McpError::internal_error(format!("Project lookup failed: {}", e), None)
    })?;

    let config = ctx.config().load();
    let bundle = resolve_effective_mandates(&config, project.as_ref());

    // Session-scoped sections (optional). Failures are non-fatal; we degrade
    // back to the file-backed bundle alone.
    let mut active_session_mandates = serde_json::Value::Array(Vec::new());
    let mut promoted_for_project = serde_json::Value::Array(Vec::new());
    if let Some(session_id_str) = params.session_id.as_deref()
        && let Ok(uuid) = Uuid::parse_str(session_id_str)
        && let Some(pool) = ctx.db().pool()
    {
        if let Ok(rows) =
            sessions::list_active_mandates(pool, Some(uuid), params.cwd.as_deref(), 50).await
        {
            active_session_mandates =
                serde_json::to_value(&rows).unwrap_or(active_session_mandates);
        }
        if let Some(p) = project.as_ref()
            && let Ok(rows) = sessions::list_durable_mandates_for_project(pool, p.id).await
        {
            promoted_for_project = serde_json::to_value(&rows).unwrap_or(promoted_for_project);
        }
    }

    let body = json!({
        "requested_project": params.project,
        "requested_cwd": params.cwd,
        "requested_session_id": params.session_id,
        "found_project": project.is_some(),
        "mandates": bundle,
        "active_session_mandates": active_session_mandates,
        "promoted_mandates_for_project": promoted_for_project,
    });

    debug!(
        tool = "mandate_context",
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed"
    );

    Ok(CallToolResult::success(vec![Content::text(
        body.to_string(),
    )]))
}
