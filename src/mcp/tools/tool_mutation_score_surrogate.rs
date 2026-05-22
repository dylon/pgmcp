//! `tool_mutation_score_surrogate` — Just et al. FSE 2014: lines-changed in
//! commits without corresponding test-file changes ≈ unmutated-survivor density.

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::MutationScoreSurrogateParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_mutation_score_surrogate(
    ctx: &SystemContext,
    params: MutationScoreSurrogateParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "mutation_score_surrogate", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;

    let limit = params.limit.unwrap_or(50);

    // For each non-test file, count commits that touched it where NO test file
    // was changed in the same commit. Divide by total commits touching it.
    let rows: Vec<(String, i64, i64)> = sqlx::query_as::<_, (String, i64, i64)>(
        "WITH commit_files AS (
            SELECT gc.id AS commit_id, gcf.file_path AS path
            FROM git_commits gc
            JOIN git_commit_files gcf ON gcf.commit_id = gc.id
            WHERE gc.project_id = $1
        ),
        commit_has_test AS (
            SELECT commit_id,
                   BOOL_OR(
                       path ~ '(^|/)(test|tests|spec|specs)(/|_)' OR
                       path ~ '(_test|_spec)\\.[a-z]+$'
                   ) AS has_test
            FROM commit_files GROUP BY commit_id
        ),
        per_file AS (
            SELECT cf.path,
                   COUNT(*)::int8 AS total_commits,
                   SUM((NOT cht.has_test)::int)::int8 AS untested_commits
            FROM commit_files cf
            JOIN commit_has_test cht ON cht.commit_id = cf.commit_id
            WHERE cf.path !~ '(^|/)(test|tests|spec|specs)(/|_)'
              AND cf.path !~ '(_test|_spec)\\.[a-z]+$'
            GROUP BY cf.path
            HAVING COUNT(*) >= 3
        )
        SELECT path, total_commits, untested_commits
        FROM per_file
        ORDER BY (untested_commits::float8 / NULLIF(total_commits, 0)) DESC NULLS LAST
        LIMIT $2",
    )
    .bind(project_id)
    .bind(limit as i64)
    .fetch_all(pool)
    .await
    .map_err(|e| {
        McpError::internal_error(format!("Mutation surrogate query failed: {}", e), None)
    })?;

    let files: Vec<_> = rows
        .into_iter()
        .map(|(p, total, untested)| {
            let ratio = if total > 0 {
                untested as f64 / total as f64
            } else {
                0.0
            };
            json!({
                "file": p,
                "total_commits": total,
                "untested_commits": untested,
                "untested_ratio": ratio,
            })
        })
        .collect();
    json_result(&json!({
        "project": params.project,
        "files": files,
        "guidance": "Untested-commit ratio approximates the share of changes that ship without a corresponding test edit. Higher = more unmutated survivors at runtime."
    }))
}
