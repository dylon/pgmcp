//! `tool_find_coupled_files` — MCP tool body, extracted from `super::super::server`.

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

pub async fn tool_find_coupled_files(
    ctx: &SystemContext,
    params: FindCoupledFilesParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().coupling_scans.fetch_add(1, Ordering::Relaxed);

    let min_coupling = params.min_coupling.unwrap_or(0.3);
    let min_commits = params.min_commits.unwrap_or(3);
    let limit = params.limit.unwrap_or(50);

    debug!(
        tool = "find_coupled_files",
        project = %params.project,
        min_coupling,
        min_commits,
        limit,
        "MCP tool invoked",
    );

    // Check if git_commit_files has data
    let has_data = ctx
        .db()
        .has_commit_files_for_project(&params.project)
        .await
        .unwrap_or(false);

    if !has_data {
        return Ok(CallToolResult::success(vec![Content::text(
            "No git commit file data found for this project. Enable git history indexing \
             by adding [git] index_history = true to the project's .pgmcp.toml, then wait \
             for the git-history-index cron job to run.",
        )]));
    }

    let mut pairs = ctx
        .db()
        .find_coupled_files(&params.project, min_coupling, min_commits)
        .await
        .map_err(|e| McpError::internal_error(format!("Coupling query failed: {}", e), None))?;

    pairs.truncate(limit as usize);

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
        "min_coupling": min_coupling,
        "min_commits": min_commits,
        "pair_count": pairs.len(),
        "coupled_pairs": pairs.iter().map(|p| serde_json::json!({
            "file_a": p.file_a,
            "file_b": p.file_b,
            "co_commits": p.co_commits,
            "commits_a": p.commits_a,
            "commits_b": p.commits_b,
            "jaccard": format!("{:.4}", p.jaccard),
        })).collect::<Vec<_>>(),
        "guidance": "High coupling (>0.7) suggests files that should be in the same module. \
                     Coupling without semantic similarity may indicate hidden dependencies.",
    });

    let json = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "find_coupled_files",
        pairs = pairs.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
