//! `tool_hot_path_audit` — files in the intersection of high PageRank,
//! high churn, and high fix_commit_ratio.
//!
//! These are the most fragile critical paths in a project. For each, the
//! tool emits a priority bucket (P0/P1/P2) and an action recommendation
//! (add integration test, freeze API, refactor) based on which dimension
//! dominates.

#![allow(unused_imports)]

use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::json;
use tracing::{debug, info};

use crate::context::SystemContext;
use crate::db::queries;
use crate::mcp::server::*;
use crate::mcp::tools::fix_helpers::pool_or_err;

pub async fn tool_hot_path_audit(
    ctx: &SystemContext,
    params: HotPathAuditParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().hot_path_audits.fetch_add(1, Ordering::Relaxed);

    let percentile_threshold = params.percentile_threshold.unwrap_or(0.9).clamp(0.0, 1.0);
    let limit = params.limit.unwrap_or(20).max(1);

    debug!(
        tool = "hot_path_audit",
        project = %params.project,
        percentile_threshold,
        limit,
        "MCP tool invoked",
    );

    let pool = pool_or_err(ctx)?;
    let rows = queries::find_hot_paths(pool, &params.project, percentile_threshold, limit)
        .await
        .map_err(|e| McpError::internal_error(format!("Hot-path query failed: {}", e), None))?;

    if rows.is_empty() {
        return Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&json!({
                "hot_paths": [],
                "summary": { "p0_count": 0, "p1_count": 0, "p2_count": 0 },
                "parameters": {
                    "project": params.project,
                    "percentile_threshold": percentile_threshold,
                    "limit": limit,
                },
                "guidance": "No files in the percentile intersection. Either the project is healthy \
                             or file_metrics/git history is incomplete — check `index_stats`.",
                "health": health_envelope(false, false),
            }))
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?,
        )]));
    }

    let graph_present = rows.iter().any(|r| r.pagerank.is_some());
    let history_present = rows.iter().any(|r| r.fix_commit_ratio.is_some());

    let mut p0 = 0_u64;
    let mut p1 = 0_u64;
    let mut p2 = 0_u64;
    let mut hot_paths: Vec<serde_json::Value> = Vec::new();
    for r in &rows {
        let composite = r.pagerank_pct + r.churn_pct + r.fix_ratio_pct;
        let priority = if composite >= 2.85 {
            "P0"
        } else if composite >= 2.70 {
            "P1"
        } else {
            "P2"
        };
        match priority {
            "P0" => p0 += 1,
            "P1" => p1 += 1,
            _ => p2 += 1,
        }

        // Action selection — order matters; first match wins.
        let instability = r.instability.unwrap_or(0.0);
        let in_degree = r.in_degree.unwrap_or(0);
        let (action, rationale) = if r.pagerank_pct >= 0.95 && in_degree >= 5 {
            (
                "freeze API",
                "Highly central + many importers. Stabilize the public surface; \
                 changes here cascade through the import graph.",
            )
        } else if instability > 0.7 {
            (
                "refactor",
                "Instability > 0.7: too much outgoing fan-out. Break into \
                 cohesive sub-units; reduce dependencies on volatile downstream.",
            )
        } else {
            (
                "add integration test",
                "Hot-path with high churn and fix-ratio. Lock in current behavior \
                 with integration tests so refactors are safe.",
            )
        };

        hot_paths.push(json!({
            "path": r.relative_path,
            "pagerank_pct": format!("{:.4}", r.pagerank_pct),
            "churn_pct": format!("{:.4}", r.churn_pct),
            "fix_ratio_pct": format!("{:.4}", r.fix_ratio_pct),
            "pagerank": r.pagerank,
            "churn_rate": r.churn_rate,
            "fix_commit_ratio": r.fix_commit_ratio,
            "bug_proneness": r.bug_proneness,
            "instability": instability,
            "in_degree": in_degree,
            "author_count": r.author_count,
            "commit_count": r.commit_count,
            "priority": priority,
            "action": action,
            "rationale": rationale,
        }));
    }

    let result = json!({
        "hot_paths": hot_paths,
        "summary": {
            "p0_count": p0,
            "p1_count": p1,
            "p2_count": p2,
        },
        "parameters": {
            "project": params.project,
            "percentile_threshold": percentile_threshold,
            "limit": limit,
        },
        "guidance": format!(
            "Files in the top {:.0}% by all of (pagerank, churn, fix_ratio). \
             P0 (composite >= 2.85) = invest immediately; P1 (>= 2.70) = next sprint; \
             P2 = monitor.",
            percentile_threshold * 100.0
        ),
        "health": health_envelope(graph_present, history_present),
    });
    let json_str = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "hot_path_audit",
        rows = rows.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json_str)]))
}

fn health_envelope(graph_present: bool, history_present: bool) -> serde_json::Value {
    json!({
        "graph_stale": !graph_present,
        "git_history_present": history_present,
    })
}
