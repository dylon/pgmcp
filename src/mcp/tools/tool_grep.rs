//! `tool_grep` — MCP tool body, extracted from `super::super::server`.

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

pub async fn tool_grep(
    ctx: &SystemContext,
    params: GrepParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().grep_searches.fetch_add(1, Ordering::Relaxed);

    let limit = params.limit.unwrap_or(10);
    info!(
        tool = "grep",
        pattern = %truncate(&params.pattern, 200),
        glob = params.glob.as_deref().unwrap_or("*"),
        limit,
        "MCP tool invoked",
    );

    let results = ctx
        .db()
        .grep_search(&params.pattern, params.glob.as_deref(), limit)
        .await
        .map_err(|e| {
            error!(tool = "grep", error = %e, "MCP tool failed");
            McpError::internal_error(format!("Grep failed: {}", e), None)
        })?;

    let count = results.len();
    let json = serde_json::to_string_pretty(&results)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "grep",
        results = count,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
