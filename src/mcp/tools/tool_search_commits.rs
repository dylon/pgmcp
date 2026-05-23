//! `tool_search_commits` — MCP tool body, extracted from `super::super::server`.

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

pub async fn tool_search_commits(
    ctx: &SystemContext,
    params: SearchCommitsParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().commit_searches.fetch_add(1, Ordering::Relaxed);

    let limit = params.limit.unwrap_or(10);
    debug!(
        tool = "search_commits",
        query = %truncate(&params.query, 200),
        limit,
        project = params.project.as_deref().unwrap_or("*"),
        "MCP tool invoked",
    );

    // Embed the query
    let embedding = ctx.embed().embed_query(&params.query).await.map_err(|e| {
        error!(tool = "search_commits", error = %e, "MCP tool failed");
        McpError::internal_error(format!("Embedding failed: {}", e), None)
    })?;

    let ef_search = ctx.config().load().vector.ef_search;
    let results = ctx
        .db()
        .semantic_search_commits(&embedding, limit, params.project.as_deref(), ef_search)
        .await
        .map_err(|e| {
            error!(tool = "search_commits", error = %e, "MCP tool failed");
            McpError::internal_error(format!("Commit search failed: {}", e), None)
        })?;

    // Shadow-ASR `touched_effects` filter: restrict the result set to
    // commits that touched at least one file containing a symbol with
    // any of the requested effects.
    let results = if let Some(touched) = params.touched_effects.as_ref()
        && !touched.is_empty()
        && let Some(pool) = ctx.db().pool()
    {
        filter_commits_by_touched_effects(pool, results, touched).await
    } else {
        results
    };
    let count = results.len();
    // Shadow-ASR channel (Phase D2b): workspace-wide effect distribution.
    let effect_breakdown: Vec<serde_json::Value> = (async {
        let Some(pool) = ctx.db().pool() else {
            return Vec::new();
        };
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT se.effect, COUNT(*)::int8
             FROM symbol_effects se
             GROUP BY se.effect
             ORDER BY se.effect",
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        rows.into_iter()
            .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
            .collect()
    })
    .await;

    let envelope = serde_json::json!({
        "results": results,
        "effect_breakdown": effect_breakdown,
    });

    let json = serde_json::to_string_pretty(&envelope)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "search_commits",
        results = count,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}

/// Restrict the commit-search result list to commits that touched at
/// least one file containing a symbol with any of the requested effects.
/// Inputs serialize to JSON objects with at least a `commit_id` field.
async fn filter_commits_by_touched_effects<R>(
    pool: &sqlx::PgPool,
    results: Vec<R>,
    touched_effects: &[String],
) -> Vec<R>
where
    R: serde::Serialize,
{
    let mut keep: Vec<R> = Vec::with_capacity(results.len());
    for r in results {
        let value = match serde_json::to_value(&r) {
            Ok(v) => v,
            Err(_) => {
                keep.push(r);
                continue;
            }
        };
        let commit_id = value.get("commit_id").and_then(|v| v.as_i64()).unwrap_or(0);
        if commit_id == 0 {
            keep.push(r);
            continue;
        }
        let hit: Option<i64> = sqlx::query_scalar(
            "SELECT 1::int8
             FROM git_commit_files gcf
             JOIN file_symbols fs ON fs.file_id = gcf.file_id
             JOIN symbol_effects se ON se.symbol_id = fs.id
             WHERE gcf.commit_id = $1 AND se.effect = ANY($2::text[])
             LIMIT 1",
        )
        .bind(commit_id)
        .bind(touched_effects)
        .fetch_optional(pool)
        .await
        .unwrap_or(None);
        if hit.is_some() {
            keep.push(r);
        }
    }
    keep
}
