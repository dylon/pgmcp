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
use crate::db::queries;
use crate::mcp::server::*;
use crate::mcp::tools::sota_helpers::project_id_or_err;

const FIND_COUPLED_FILES_MAX_LIMIT: i32 = 200;
const FIND_COUPLED_FILES_MAX_MIN_COMMITS: i32 = 10_000;

fn normalize_find_coupled_params(
    params: &FindCoupledFilesParams,
) -> Result<(String, f64, i32, i32), McpError> {
    let project = params.project.trim();
    if project.is_empty() {
        return Err(McpError::invalid_params("project must be non-empty", None));
    }

    let raw_min_coupling = params.min_coupling.unwrap_or(0.3);
    if !raw_min_coupling.is_finite() {
        return Err(McpError::invalid_params(
            "min_coupling must be a finite number",
            None,
        ));
    }
    let min_coupling = raw_min_coupling.clamp(0.0, 1.0);
    let min_commits = params
        .min_commits
        .unwrap_or(3)
        .clamp(1, FIND_COUPLED_FILES_MAX_MIN_COMMITS);
    let limit = params
        .limit
        .unwrap_or(50)
        .clamp(1, FIND_COUPLED_FILES_MAX_LIMIT);

    Ok((project.to_string(), min_coupling, min_commits, limit))
}

pub async fn tool_find_coupled_files(
    ctx: &SystemContext,
    params: FindCoupledFilesParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().coupling_scans.fetch_add(1, Ordering::Relaxed);

    let (project, min_coupling, min_commits, limit) = normalize_find_coupled_params(&params)?;

    debug!(
        tool = "find_coupled_files",
        project = %project,
        min_coupling,
        min_commits,
        limit,
        "MCP tool invoked",
    );

    let pool = ctx.db().pool();
    let mut pairs = if let Some(pool) = pool {
        let project_id = project_id_or_err(ctx, &project).await?;
        let has_data = queries::has_commit_files_for_project_id(pool, project_id)
            .await
            .map_err(|e| McpError::internal_error(format!("Git data check failed: {}", e), None))?;

        if !has_data {
            return Ok(CallToolResult::success(vec![Content::text(
                "No git commit file data found for this project. Enable git history indexing \
                 by adding [git] index_history = true to the project's .pgmcp.toml, then wait \
                 for the git-history-index cron job to run.",
            )]));
        }

        queries::find_coupled_files_by_project_id(pool, project_id, min_coupling, min_commits)
            .await
            .map_err(|e| McpError::internal_error(format!("Coupling query failed: {}", e), None))?
    } else {
        let has_data = ctx
            .db()
            .has_commit_files_for_project(&project)
            .await
            .unwrap_or(false);

        if !has_data {
            return Ok(CallToolResult::success(vec![Content::text(
                "No git commit file data found for this project. Enable git history indexing \
                 by adding [git] index_history = true to the project's .pgmcp.toml, then wait \
                 for the git-history-index cron job to run.",
            )]));
        }

        ctx.db()
            .find_coupled_files(&project, min_coupling, min_commits)
            .await
            .map_err(|e| McpError::internal_error(format!("Coupling query failed: {}", e), None))?
    };

    pairs.truncate(limit as usize);

    // Shadow-ASR channel (Phase D2b): per-effect symbol-count breakdown
    // for the project. Universal enrichment — every tool benefits from
    // surfacing the effect distribution alongside its primary output.
    // Gracefully degrades to empty when the project lookup or
    // shadow-ASR data isn't populated.
    let effect_breakdown: Vec<serde_json::Value> = (async {
        let Some(pool) = pool else {
            return Vec::new();
        };
        match project_id_or_err(ctx, &project).await {
            Ok(project_id) => {
                crate::mcp::tools::sema_helpers::effects::effect_counts(pool, project_id)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
                    .collect()
            }
            Err(_) => Vec::new(),
        }
    })
    .await;

    let result = serde_json::json!({
        "effect_breakdown": effect_breakdown,
        "project": project,
        "min_coupling": min_coupling,
        "min_commits": min_commits,
        "limit": limit,
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
