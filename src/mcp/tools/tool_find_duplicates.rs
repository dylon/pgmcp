//! `tool_find_duplicates` — MCP tool body, extracted from `super::super::server`.

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

pub async fn tool_find_duplicates(
    ctx: &SystemContext,
    params: FindDuplicatesParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let min_sim = params.min_similarity.unwrap_or(0.90);
    let min_projects = params.min_projects.unwrap_or(2);
    let limit = params.limit.unwrap_or(20);
    info!(
        tool = "find_duplicates",
        min_similarity = min_sim,
        min_projects,
        language = params.language.as_deref().unwrap_or("*"),
        limit,
        "MCP tool invoked",
    );

    let pairs = ctx
        .db()
        .find_duplicate_file_pairs(
            min_sim,
            params.language.as_deref(),
            limit * 5,
            params.include_same_repo.unwrap_or(false),
        )
        .await
        .map_err(|e| McpError::internal_error(format!("Duplicate query failed: {}", e), None))?;

    let clusters = cluster_file_pairs(&pairs, min_projects);
    let limited: Vec<_> = clusters.into_iter().take(limit as usize).collect();

    let json = serde_json::to_string_pretty(&limited)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "find_duplicates",
        clusters = limited.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
