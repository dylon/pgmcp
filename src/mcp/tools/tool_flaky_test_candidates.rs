//! `tool_flaky_test_candidates` — Heuristic: commits mentioning "fix flaky" /
//! "intermittent" / "retry" near test edits (SOTA Phase 4.7,
//! Luo et al. FSE 2014; Lam et al. ASE 2019).

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::FlakyTestCandidatesParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_flaky_test_candidates(
    ctx: &SystemContext,
    params: FlakyTestCandidatesParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "flaky_test_candidates", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;

    let limit = params.limit.unwrap_or(30);

    let rows: Vec<(String, i64, i64)> = sqlx::query_as::<_, (String, i64, i64)>(
        "WITH suspicious AS (
            SELECT gc.id AS commit_id, gc.subject, gc.body
            FROM git_commits gc
            WHERE gc.project_id = $1
              AND (
                gc.subject ~* '(flaky|intermittent|race|retry|timing|sporadic|hang|deadlock)'
                OR gc.body ~* '(flaky|intermittent|race|retry|timing|sporadic|hang|deadlock)'
              )
        ),
        per_file AS (
            SELECT gcf.file_path AS path,
                   COUNT(DISTINCT s.commit_id)::int8 AS flaky_commits,
                   COUNT(DISTINCT gcf.commit_id)::int8 AS total_commits
            FROM git_commit_files gcf
            JOIN git_commits gc ON gcf.commit_id = gc.id AND gc.project_id = $1
            LEFT JOIN suspicious s ON s.commit_id = gcf.commit_id
            WHERE gcf.file_path ~ '(^|/)(test|tests|spec|specs)(/|_)'
               OR gcf.file_path ~ '(_test|_spec)\\.[a-z]+$'
            GROUP BY gcf.file_path
            HAVING COUNT(DISTINCT s.commit_id) >= 1
        )
        SELECT path, flaky_commits, total_commits FROM per_file
        ORDER BY flaky_commits DESC, total_commits DESC
        LIMIT $2",
    )
    .bind(project_id)
    .bind(limit as i64)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Flaky query failed: {}", e), None))?;

    let files: Vec<_> = rows
        .into_iter()
        .map(|(p, f, t)| {
            json!({
                "test_file": p,
                "suspicious_commits": f,
                "total_commits": t,
                "ratio": if t > 0 { f as f64 / t as f64 } else { 0.0 },
            })
        })
        .collect();
    json_result(&json!({
        "project": params.project,
        "test_files": files,
        "guidance": "Test files frequently touched by commits whose messages mention flakiness/race/retry/timing are flake candidates. Inspect them for time-, random-, or threading-dependent assertions."
    }))
}
