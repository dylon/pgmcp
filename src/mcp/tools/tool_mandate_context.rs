//! `tool_mandate_context` — effective workspace/project mandates.

use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::json;
use tracing::{debug, error, info};

use crate::context::SystemContext;
use crate::mandates::{resolve_effective_mandates, resolve_project_for_mandates};
use crate::mcp::server::MandateContextParams;

pub async fn tool_mandate_context(
    ctx: &SystemContext,
    params: MandateContextParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    info!(
        tool = "mandate_context",
        project = ?params.project,
        cwd = ?params.cwd,
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
    let body = json!({
        "requested_project": params.project,
        "requested_cwd": params.cwd,
        "found_project": project.is_some(),
        "mandates": bundle,
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
