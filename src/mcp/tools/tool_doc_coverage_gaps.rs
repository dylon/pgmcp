//! `tool_doc_coverage_gaps` — MCP tool body, extracted from `super::super::server`.

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
use crate::db::queries;
use crate::mcp::server::*;

pub async fn tool_doc_coverage_gaps(
    ctx: &SystemContext,
    params: DocCoverageGapsParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .doc_coverage_scans
        .fetch_add(1, Ordering::Relaxed);

    let project = params.project.trim();
    if project.is_empty() {
        return Err(McpError::invalid_params("project must be non-empty", None));
    }

    debug!(
        tool = "doc_coverage_gaps",
        project = %project,
        "MCP tool invoked",
    );

    let pool = ctx.db().pool();
    let resolved_project_id = match pool {
        Some(pool) => resolve_unique_project_id(pool, project).await?,
        None => None,
    };

    let rows = match (pool, resolved_project_id) {
        (Some(pool), Some(project_id)) => {
            queries::get_doc_topic_coverage_by_project_id(pool, project_id).await
        }
        (Some(_), None) => Ok(Vec::new()),
        (None, _) => ctx.db().get_doc_topic_coverage(project).await,
    }
    .map_err(|e| McpError::internal_error(format!("Coverage query failed: {}", e), None))?;

    if rows.is_empty() {
        return Ok(CallToolResult::success(vec![Content::text(
            "No topic assignments found. Run discover_topics first.",
        )]));
    }

    let mut topics: Vec<serde_json::Value> = Vec::with_capacity(rows.len());
    let mut total_doc_chunks: i64 = 0;
    let mut total_code_chunks: i64 = 0;

    for row in &rows {
        total_doc_chunks += row.doc_chunks;
        total_code_chunks += row.code_chunks;

        let total = row.doc_chunks + row.code_chunks;
        let doc_ratio = if total > 0 {
            row.doc_chunks as f64 / total as f64
        } else {
            0.0
        };

        let status = if doc_ratio > 0.30 {
            "well-documented"
        } else if doc_ratio > 0.05 {
            "under-documented"
        } else {
            "undocumented"
        };

        topics.push(serde_json::json!({
            "topic_id": row.topic_id,
            "label": row.label,
            "keywords": row.keywords,
            "doc_chunks": row.doc_chunks,
            "code_chunks": row.code_chunks,
            "doc_ratio": format!("{:.2}", doc_ratio),
            "status": status,
        }));
    }

    // Sort by doc_ratio ascending (worst first)
    topics.sort_by(|a, b| {
        let ra: f64 = a["doc_ratio"]
            .as_str()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0.0);
        let rb: f64 = b["doc_ratio"]
            .as_str()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0.0);
        ra.partial_cmp(&rb).unwrap_or(std::cmp::Ordering::Equal)
    });

    // Shadow-ASR channel (Phase D2b): per-effect symbol-count breakdown
    // for the project. Universal enrichment — every tool benefits from
    // surfacing the effect distribution alongside its primary output.
    // Gracefully degrades to empty when the project lookup or
    // shadow-ASR data isn't populated.
    let effect_breakdown: Vec<serde_json::Value> = match (pool, resolved_project_id) {
        (Some(pool), Some(project_id)) => {
            crate::mcp::tools::sema_helpers::effects::effect_counts(pool, project_id)
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
        "total_doc_chunks": total_doc_chunks,
        "total_code_chunks": total_code_chunks,
        "topic_count": topics.len(),
        "topics": topics,
        "guidance": "Topics marked 'undocumented' have code with no corresponding \
                     markdown documentation. Focus on topics with many code chunks \
                     but zero doc chunks. Consider creating documentation for these areas.",
    });

    let json = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "doc_coverage_gaps",
        topics = topics.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}

async fn resolve_unique_project_id(
    pool: &sqlx::PgPool,
    project: &str,
) -> Result<Option<i32>, McpError> {
    let ids: Vec<i32> = sqlx::query_scalar("SELECT id FROM projects WHERE name = $1 LIMIT 2")
        .bind(project)
        .fetch_all(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("project lookup failed: {e}"), None))?;

    match ids.as_slice() {
        [] => Ok(None),
        [id] => Ok(Some(*id)),
        _ => Err(McpError::invalid_params(
            format!("project name '{project}' is not unique"),
            None,
        )),
    }
}
