//! `tool_text_search` — MCP tool body, extracted from `super::super::server`.

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

pub async fn tool_text_search(
    ctx: &SystemContext,
    params: TextSearchParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().text_searches.fetch_add(1, Ordering::Relaxed);

    let limit = params.limit.unwrap_or(10);
    info!(
        tool = "text_search",
        query = %truncate(&params.query, 200),
        limit,
        language = params.language.as_deref().unwrap_or("*"),
        "MCP tool invoked",
    );

    let results = ctx
        .db()
        .text_search(
            &params.query,
            limit,
            params.language.as_deref(),
            params.dedupe_worktrees.unwrap_or(false),
        )
        .await
        .map_err(|e| {
            error!(tool = "text_search", error = %e, "MCP tool failed");
            McpError::internal_error(format!("Search failed: {}", e), None)
        })?;

    let count = results.len();
    let json = serde_json::to_string_pretty(&results)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "text_search",
        results = count,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
