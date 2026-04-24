//! `tool_project_tree` — MCP tool body, extracted from `super::super::server`.

#![allow(unused_imports)]

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, LoggingLevel};
use serde_json::json;
use tracing::{debug, error, info, warn};

use crate::context::SystemContext;
use crate::mcp::server::*;

pub async fn tool_project_tree(
    ctx: &SystemContext,
    params: ProjectTreeParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);

    let depth = params.depth.unwrap_or(5);
    info!(
        tool = "project_tree",
        project = %params.project,
        depth,
        "MCP tool invoked",
    );

    let paths = ctx
        .db()
        .project_tree(&params.project, depth)
        .await
        .map_err(|e| {
            error!(tool = "project_tree", error = %e, "MCP tool failed");
            McpError::internal_error(format!("Query failed: {}", e), None)
        })?;

    let count = paths.len();
    debug!(
        tool = "project_tree",
        results = count,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    let tree = paths.join("\n");
    Ok(CallToolResult::success(vec![Content::text(tree)]))
}
