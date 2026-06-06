//! `tool_find_orphans` — MCP tool body, extracted from `super::super::server`.

use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use tracing::debug;

use crate::context::SystemContext;
use crate::mcp::server::*;
use crate::mcp::tools::sota_helpers::project_id_or_err;

fn normalize_optional(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub async fn tool_find_orphans(
    ctx: &SystemContext,
    params: FindOrphansParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().orphan_scans.fetch_add(1, Ordering::Relaxed);

    let project = match params.project {
        Some(raw) if raw.trim().is_empty() => {
            return Err(McpError::invalid_params(
                "project must be non-empty when supplied",
                None,
            ));
        }
        other => normalize_optional(other),
    };
    let language = normalize_optional(params.language);
    let limit = params.limit.unwrap_or(50).clamp(1, 1000);
    let detail = params
        .detail
        .as_deref()
        .map(str::trim)
        .filter(|detail| !detail.is_empty())
        .unwrap_or("files");
    if !matches!(detail, "files" | "chunks") {
        return Err(McpError::invalid_params(
            format!(
                "Unknown detail '{}': expected one of files | chunks",
                detail
            ),
            None,
        ));
    }

    debug!(
        tool = "find_orphans",
        project = project.as_deref().unwrap_or("*"),
        language = language.as_deref().unwrap_or("*"),
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

    let project_id = match project.as_deref() {
        Some(name) => Some(project_id_or_err(ctx, name).await?),
        None => None,
    };

    let json = if detail == "chunks" {
        let chunks = if let Some(pool) = ctx.db().pool() {
            crate::db::queries::find_orphan_chunks_by_project_id(
                pool,
                project_id,
                language.as_deref(),
                limit,
            )
            .await
        } else {
            ctx.db()
                .find_orphan_chunks(project.as_deref(), language.as_deref(), limit)
                .await
        }
        .map_err(|e| McpError::internal_error(format!("Orphan query failed: {}", e), None))?;

        let result = serde_json::json!({
            "project": project,
            "language": language,
            "detail": "chunks",
            "limit": limit,
            "orphan_count": chunks.len(),
            "orphans": chunks,
            "guidance": "Orphan chunks are code not assigned to any semantic topic. \
                         They may be utility functions, one-off scripts, or code needing refactoring.",
        });
        serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?
    } else {
        let mut files = if let Some(pool) = ctx.db().pool() {
            crate::db::queries::find_orphan_file_summary_by_project_id(
                pool,
                project_id,
                language.as_deref(),
                limit,
            )
            .await
        } else {
            ctx.db().find_orphan_file_summary(project.as_deref()).await
        }
        .map_err(|e| McpError::internal_error(format!("Orphan query failed: {}", e), None))?;
        if let Some(language) = language.as_deref() {
            files.retain(|row| row.language == language);
        }
        files.truncate(limit as usize);

        // Shadow-ASR channel (Phase D2b): per-effect symbol-count breakdown
        // for the project. Universal enrichment — every tool benefits from
        // surfacing the effect distribution alongside its primary output.
        // Gracefully degrades to empty when the project lookup or
        // shadow-ASR data isn't populated.
        let effect_breakdown: Vec<serde_json::Value> = match (ctx.db().pool(), project_id) {
            (Some(pool), Some(pid)) => {
                crate::mcp::tools::sema_helpers::effects::effect_counts(pool, pid)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
                    .collect()
            }
            _ => Vec::new(),
        };

        let result = serde_json::json!({
            "effect_breakdown": effect_breakdown,
            "project": project,
            "language": language,
            "detail": "files",
            "limit": limit,
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
