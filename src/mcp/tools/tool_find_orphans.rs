//! `tool_find_orphans` — MCP tool body, extracted from `super::super::server`.

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

pub async fn tool_find_orphans(
    ctx: &SystemContext,
    params: FindOrphansParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().orphan_scans.fetch_add(1, Ordering::Relaxed);

    let limit = params.limit.unwrap_or(50);
    let detail = params.detail.as_deref().unwrap_or("files");

    debug!(
        tool = "find_orphans",
        project = params.project.as_deref().unwrap_or("*"),
        language = params.language.as_deref().unwrap_or("*"),
        detail,
        limit,
        "MCP tool invoked",
    );

    // Check if topics have been computed
    let has_topics = ctx.db().has_topic_assignments().await.unwrap_or(false);

    if !has_topics {
        return Ok(CallToolResult::success(vec![Content::text(
            "No topic assignments found. Run discover_topics first to compute semantic \
             clusters, then find_orphans will identify chunks not assigned to any topic.",
        )]));
    }

    let json = if detail == "chunks" {
        let chunks = ctx
            .db()
            .find_orphan_chunks(params.project.as_deref(), params.language.as_deref(), limit)
            .await
            .map_err(|e| McpError::internal_error(format!("Orphan query failed: {}", e), None))?;

        let result = serde_json::json!({
            "detail": "chunks",
            "orphan_count": chunks.len(),
            "orphans": chunks,
            "guidance": "Orphan chunks are code not assigned to any semantic topic. \
                         They may be utility functions, one-off scripts, or code needing refactoring.",
        });
        serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?
    } else {
        let files = ctx
            .db()
            .find_orphan_file_summary(params.project.as_deref())
            .await
            .map_err(|e| McpError::internal_error(format!("Orphan query failed: {}", e), None))?;

        let result = serde_json::json!({
            "detail": "files",
            "file_count": files.len(),
            "files": files,
            "guidance": "Files with high orphan_pct have code that doesn't fit any discovered \
                         semantic pattern. Consider refactoring or reviewing these files.",
        });
        serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?
    };

    debug!(
        tool = "find_orphans",
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
