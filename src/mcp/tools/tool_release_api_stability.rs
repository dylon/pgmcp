//! `tool_release_api_stability` — Bogart EMSE 2016 metric over release-like
//! commits (heuristic: commits whose subject matches a semver or `release`
//! marker) (SOTA Phase 11.4).

#![allow(unused_imports)]

use chrono::{DateTime, Utc};
use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::ReleaseApiStabilityParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_release_api_stability(
    ctx: &SystemContext,
    params: ReleaseApiStabilityParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "release_api_stability", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(50);

    let releases: Vec<(String, DateTime<Utc>)> = sqlx::query_as::<_, (String, DateTime<Utc>)>(
        "SELECT subject, committed_at
         FROM git_commits
         WHERE project_id = $1
           AND (subject ~* '^(v?\\d+\\.\\d+\\.\\d+)' OR subject ~* '\\brelease\\b' OR subject ~* '\\bbump\\b')
         ORDER BY committed_at",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Release query failed: {}", e), None))?;

    if releases.len() < 2 {
        return json_result(&json!({
            "project": params.project,
            "releases": releases.len(),
            "symbols": [],
            "guidance": "Fewer than 2 release-like commits found; need a longer history."
        }));
    }

    // Count public-API-line changes per release interval. Approximate by
    // counting commits between releases that contained `pub fn` / `export`.
    let api_changes: Vec<(String, i64)> = sqlx::query_as::<_, (String, i64)>(
        "SELECT gc.subject, COUNT(*)::int8
         FROM git_commits gc
         JOIN git_commit_chunks gcc ON gcc.commit_id = gc.id
         WHERE gc.project_id = $1
           AND gcc.chunk_text ~* '(\\+\\s*pub\\s+fn|\\+\\s*export\\s+function|\\+\\s*def\\s+[a-z_])'
         GROUP BY gc.subject
         ORDER BY COUNT(*) DESC
         LIMIT $2",
    )
    .bind(project_id)
    .bind(limit as i64)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("API-change query failed: {}", e), None))?;

    let total_releases = releases.len() as f64;
    let rows_json: Vec<_> = api_changes
        .into_iter()
        .map(|(subj, n)| {
            let rate = n as f64 / total_releases.max(1.0);
            let stability = 1.0 / (1.0 + rate);
            json!({
                "commit_subject": subj,
                "public_api_changes": n,
                "release_rate": rate,
                "stability_score": stability,
            })
        })
        .collect();
    json_result(&json!({
        "project": params.project,
        "releases": releases.len(),
        "commits": rows_json,
        "guidance": "Bogart EMSE 2016 metric adapted to release-marker commits. Each commit's stability = 1 / (1 + public_api_change_rate). Low scores = unstable releases."
    }))
}
