//! `tool_search_commits` — MCP tool body, extracted from `super::super::server`.

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

pub async fn tool_search_commits(
    ctx: &SystemContext,
    params: SearchCommitsParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().commit_searches.fetch_add(1, Ordering::Relaxed);

    let limit = params.limit.unwrap_or(10);
    debug!(
        tool = "search_commits",
        query = %truncate(&params.query, 200),
        limit,
        project = params.project.as_deref().unwrap_or("*"),
        "MCP tool invoked",
    );

    // Embed the query
    let embedding = ctx.embed().embed_query(&params.query).await.map_err(|e| {
        error!(tool = "search_commits", error = %e, "MCP tool failed");
        McpError::internal_error(format!("Embedding failed: {}", e), None)
    })?;

    let ef_search = ctx.config().load().vector.ef_search;
    let results = ctx
        .db()
        .semantic_search_commits(&embedding, limit, params.project.as_deref(), ef_search)
        .await
        .map_err(|e| {
            error!(tool = "search_commits", error = %e, "MCP tool failed");
            McpError::internal_error(format!("Commit search failed: {}", e), None)
        })?;

    let count = results.len();
    let json = serde_json::to_string_pretty(&results)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "search_commits",
        results = count,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
