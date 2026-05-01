//! `tool_merge_conflict_risk` — peer-overlap on a branch's files.
//!
//! Aggregates `git_commits JOIN git_commit_files` filtered by author_date
//! within `window_days` AND `file_path = ANY($branch_files)`, excluding the
//! caller's email when supplied. Per file: distinct other authors + recent
//! commit count. Maps to risk tier + recommendation.

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

pub async fn tool_merge_conflict_risk(
    ctx: &SystemContext,
    params: MergeConflictRiskParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().merge_risk_scans.fetch_add(1, Ordering::Relaxed);

    if params.branch_files.is_empty() {
        return Err(McpError::invalid_params(
            "merge_conflict_risk requires at least one file in `branch_files`".to_string(),
            None,
        ));
    }
    let window_days = params.window_days.unwrap_or(14).max(1);
    let limit = params.limit.unwrap_or(50).max(1) as usize;

    info!(
        tool = "merge_conflict_risk",
        project = %params.project,
        files = params.branch_files.len(),
        window_days,
        excluded = params.exclude_email.as_deref().unwrap_or(""),
        "MCP tool invoked",
    );

    let pool = pool_or_err(ctx)?;
    let rows = queries::find_merge_conflict_risks(
        pool,
        &params.project,
        &params.branch_files,
        window_days,
        params.exclude_email.as_deref(),
    )
    .await
    .map_err(|e| McpError::internal_error(format!("Merge-risk query failed: {}", e), None))?;

    let git_history_present = !rows.is_empty();

    let mut risks: Vec<serde_json::Value> = Vec::new();
    for r in rows.iter().take(limit) {
        let risk = if r.distinct_recent_authors >= 3 {
            "high"
        } else if r.distinct_recent_authors >= 1 {
            "medium"
        } else {
            "low"
        };
        let recommendation = match risk {
            "high" => format!(
                "{} other authors recently touched this file. Rebase early; coordinate with: {:?}.",
                r.distinct_recent_authors, r.top_other_authors
            ),
            "medium" => format!(
                "{} other authors recently touched. Merge soon and watch for conflicts.",
                r.distinct_recent_authors
            ),
            _ => "No other authors active in the window — ship freely.".to_string(),
        };
        risks.push(json!({
            "path": r.file_path,
            "distinct_recent_authors": r.distinct_recent_authors,
            "recent_commits": r.recent_commits,
            "top_other_authors": r.top_other_authors.clone(),
            "risk": risk,
            "recommendation": recommendation,
        }));
    }

    let result = json!({
        "project": params.project,
        "window_days": window_days,
        "branch_file_count": params.branch_files.len(),
        "risks": risks,
        "guidance": "Files with ≥3 other authors in the window are high-risk; coordinate before \
                     merging. Files absent from the result had no peer activity in the window.",
        "health": json!({
            "git_history_present": git_history_present,
        }),
    });
    let json_str = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "merge_conflict_risk",
        rows = risks.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json_str)]))
}
