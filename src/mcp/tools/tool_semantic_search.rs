//! `tool_semantic_search` — MCP tool body, extracted from `super::super::server`.

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

pub async fn tool_semantic_search(
    ctx: &SystemContext,
    params: SemanticSearchParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .semantic_searches
        .fetch_add(1, Ordering::Relaxed);

    let limit = params.limit.unwrap_or(10);
    info!(
        tool = "semantic_search",
        query = %truncate(&params.query, 200),
        limit,
        language = params.language.as_deref().unwrap_or("*"),
        project = params.project.as_deref().unwrap_or("*"),
        "MCP tool invoked",
    );

    // Embed the query
    let embedding = ctx.embed().embed_query(&params.query).await.map_err(|e| {
        error!(tool = "semantic_search", error = %e, "MCP tool failed");
        McpError::internal_error(format!("Embedding failed: {}", e), None)
    })?;

    let ef_search = ctx.config().load().vector.ef_search;
    let results = ctx
        .db()
        .semantic_search(
            &embedding,
            limit,
            params.language.as_deref(),
            params.project.as_deref(),
            ef_search,
        )
        .await
        .map_err(|e| {
            error!(tool = "semantic_search", error = %e, "MCP tool failed");
            McpError::internal_error(format!("Search failed: {}", e), None)
        })?;

    let count = results.len();
    let json = serde_json::to_string_pretty(&results)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "semantic_search",
        results = count,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
