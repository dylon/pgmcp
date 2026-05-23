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

        // Shadow-ASR channel (Phase D2b): per-effect symbol-count breakdown
        // for the project. Universal enrichment — every tool benefits from
        // surfacing the effect distribution alongside its primary output.
        // Gracefully degrades to empty when the project lookup or
        // shadow-ASR data isn't populated.
        let effect_breakdown: Vec<serde_json::Value> = (async {
            let Some(pool) = ctx.db().pool() else {
                return Vec::new();
            };
            let project_id_opt: Option<i32> =
                sqlx::query_scalar("SELECT id FROM projects WHERE name = $1")
                    .bind(&params.project)
                    .fetch_optional(pool)
                    .await
                    .unwrap_or(None);
            match project_id_opt {
                Some(pid) => crate::mcp::tools::sema_helpers::effects::effect_counts(pool, pid)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
                    .collect(),
                None => Vec::new(),
            }
        })
        .await;

        let result = serde_json::json!({
        "effect_breakdown": effect_breakdown,
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
