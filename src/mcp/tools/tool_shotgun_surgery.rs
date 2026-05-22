//! `tool_shotgun_surgery` — Detect symbols whose changes ripple across
//! many files per commit (SOTA Phase 10.3).
//!
//! Distinct from `tool_shotgun_surgery_fix` (recommendation tool) — this
//! detects the smell from git history.

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::ShotgunSurgeryParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_shotgun_surgery(
    ctx: &SystemContext,
    params: ShotgunSurgeryParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "shotgun_surgery", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let since_commits = params.since_commits.unwrap_or(50);
    let min_files = params.min_files.unwrap_or(4) as i64;
    let limit = params.limit.unwrap_or(30);

    // For each commit in the recent window, count distinct files. Group
    // commits by their subject+symbol-token to identify shotgun-surgery commits.
    let rows: Vec<(String, i64)> = sqlx::query_as::<_, (String, i64)>(
        "WITH recent AS (
            SELECT gc.id, gc.subject
            FROM git_commits gc
            WHERE gc.project_id = $1
            ORDER BY gc.committed_at DESC
            LIMIT $2
        ),
        scope AS (
            SELECT r.subject, COUNT(DISTINCT gcf.file_path)::int8 AS file_count
            FROM recent r
            JOIN git_commit_files gcf ON gcf.commit_id = r.id
            GROUP BY r.subject
            HAVING COUNT(DISTINCT gcf.file_path) >= $3
        )
        SELECT subject, file_count FROM scope
        ORDER BY file_count DESC
        LIMIT $4",
    )
    .bind(project_id)
    .bind(since_commits as i64)
    .bind(min_files)
    .bind(limit as i64)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Shotgun query failed: {}", e), None))?;

    let commits: Vec<_> = rows
        .into_iter()
        .map(|(subj, n)| json!({"subject": subj, "files_touched": n}))
        .collect();
    json_result(&json!({
        "project": params.project,
        "window_commits": since_commits,
        "min_files": min_files,
        "shotgun_surgery_commits": commits,
        "guidance": "Commits touching many files indicate scattered responsibility. The recipe for shotgun surgery: small functional change requires N edits across N files. Consolidate the affected concern."
    }))
}
