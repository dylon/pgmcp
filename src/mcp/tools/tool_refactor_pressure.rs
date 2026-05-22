//! `tool_refactor_pressure` — Per-file ratio of non-test churn vs test churn
//! (SOTA Phase 11.1, Tufano et al. ICSE 2015).

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::RefactorPressureParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_refactor_pressure(
    ctx: &SystemContext,
    params: RefactorPressureParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "refactor_pressure", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let since_days = params.since_days.unwrap_or(180) as i64;
    let limit = params.limit.unwrap_or(30);

    let rows: Vec<(String, i64, i64, f64)> = sqlx::query_as::<_, (String, i64, i64, f64)>(
        "WITH window_commits AS (
            SELECT gc.id, gc.committed_at, gcf.file_path AS path
            FROM git_commits gc
            JOIN git_commit_files gcf ON gcf.commit_id = gc.id
            WHERE gc.project_id = $1
              AND gc.committed_at > NOW() - ($2::int8 || ' days')::interval
        ),
        per_file AS (
            SELECT path,
                   COUNT(*)::int8 AS commits,
                   SUM(CASE WHEN path ~ '(^|/)(test|tests|spec|specs)(/|_)' OR path ~ '(_test|_spec)\\.[a-z]+$' THEN 0 ELSE 1 END)::int8 AS non_test_commits,
                   SUM(CASE WHEN path ~ '(^|/)(test|tests|spec|specs)(/|_)' OR path ~ '(_test|_spec)\\.[a-z]+$' THEN 1 ELSE 0 END)::int8 AS test_commits
            FROM window_commits
            GROUP BY path
        )
        SELECT path, non_test_commits, test_commits,
               (non_test_commits::float8 / NULLIF(test_commits, 0)) AS pressure
        FROM per_file
        WHERE non_test_commits >= 3
        ORDER BY pressure DESC NULLS LAST
        LIMIT $3",
    )
    .bind(project_id)
    .bind(since_days)
    .bind(limit as i64)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Pressure query failed: {}", e), None))?;

    let files: Vec<_> = rows
        .into_iter()
        .map(|(p, nt, t, pr)| {
            json!({
                "file": p,
                "non_test_commits": nt,
                "test_commits": t,
                "pressure": pr,
            })
        })
        .collect();
    json_result(&json!({
        "project": params.project,
        "since_days": since_days,
        "files": files,
        "guidance": "Pressure = non_test_commits / test_commits over the window. High values mean changes ship without test coverage churn (refactor risk)."
    }))
}
