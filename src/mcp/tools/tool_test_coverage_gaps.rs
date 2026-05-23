//! `tool_test_coverage_gaps` — MCP tool body, extracted from `super::super::server`.

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

pub async fn tool_test_coverage_gaps(
    ctx: &SystemContext,
    params: TestCoverageGapsParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().coverage_scans.fetch_add(1, Ordering::Relaxed);

    debug!(
        tool = "test_coverage_gaps",
        project = %params.project,
        "MCP tool invoked",
    );

    let rows = ctx
        .db()
        .get_test_topic_coverage(&params.project)
        .await
        .map_err(|e| McpError::internal_error(format!("Coverage query failed: {}", e), None))?;

    if rows.is_empty() {
        return Ok(CallToolResult::success(vec![Content::text(
            "No topic assignments found. Run discover_topics first.",
        )]));
    }

    let mut topics: Vec<serde_json::Value> = Vec::with_capacity(rows.len());
    let mut total_test_chunks: i64 = 0;
    let mut total_impl_chunks: i64 = 0;

    for row in &rows {
        total_test_chunks += row.test_chunks;
        total_impl_chunks += row.impl_chunks;

        let total = row.test_chunks + row.impl_chunks;
        let test_ratio = if total > 0 {
            row.test_chunks as f64 / total as f64
        } else {
            0.0
        };

        let status = if test_ratio > 0.3 {
            "well-tested"
        } else if test_ratio > 0.01 {
            "under-tested"
        } else {
            "untested"
        };

        topics.push(serde_json::json!({
            "topic_id": row.topic_id,
            "label": row.label,
            "impl_chunks": row.impl_chunks,
            "test_chunks": row.test_chunks,
            "test_ratio": format!("{:.2}", test_ratio),
            "status": status,
        }));
    }

    // Sort by test ratio ascending (worst first)
    topics.sort_by(|a, b| {
        let ra: f64 = a["test_ratio"]
            .as_str()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0.0);
        let rb: f64 = b["test_ratio"]
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
        "project": params.project,
        "total_impl_chunks": total_impl_chunks,
        "total_test_chunks": total_test_chunks,
        "topic_count": topics.len(),
        "topics": topics,
        "guidance": "Topics with 0% test coverage are highest priority for test development. \
                     Focus on topics with many implementation chunks but no corresponding \
                     test chunks.",
    });

    let json = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "test_coverage_gaps",
        topics = topics.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
