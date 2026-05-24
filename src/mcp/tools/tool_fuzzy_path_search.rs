//! `tool_fuzzy_path_search` (Phase 8, P13.4 real implementation).
//!
//! Same shape as `tool_fuzzy_symbol_search` but over
//! `indexed_files.relative_path`. P13.4 changes:
//!
//! - Mandatory `project` filter (the prior `SELECT relative_path
//!   FROM indexed_files LIMIT 5000` spanned every indexed project).
//! - No artificial `LIMIT` on the vocabulary fetch.
//! - Articulatory re-rank for tiebreakers.

use std::sync::atomic::Ordering;

use libdictenstein::dynamic_dawg_char::DynamicDawgChar;
use liblevenshtein::transducer::Transducer;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::fuzzy::phonetic::articulatory_distance_score;
use crate::mcp::server::FuzzyPathSearchParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

pub async fn run(
    ctx: &SystemContext,
    params: FuzzyPathSearchParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let rows: Vec<(String,)> = if let Some(project_name) = params.project.as_deref() {
        sqlx::query_as::<_, (String,)>(
            "SELECT f.relative_path
             FROM indexed_files f
             JOIN projects p ON f.project_id = p.id
             WHERE p.name = $1
               AND f.relative_path IS NOT NULL
               AND length(f.relative_path) > 0",
        )
        .bind(project_name)
        .fetch_all(pool)
        .await
    } else {
        sqlx::query_as::<_, (String,)>(
            "SELECT relative_path
             FROM indexed_files
             WHERE relative_path IS NOT NULL
               AND length(relative_path) > 0",
        )
        .fetch_all(pool)
        .await
    }
    .map_err(|e| McpError::internal_error(format!("path fetch: {e}"), None))?;

    let paths: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
    if paths.is_empty() {
        return json_result(&json!({
            "query": params.query,
            "project": params.project,
            "max_distance": params.max_distance.unwrap_or(2),
            "hits": Vec::<serde_json::Value>::new(),
        }));
    }

    let dict: DynamicDawgChar<()> = DynamicDawgChar::from_terms(paths);
    let xducer = Transducer::with_transposition(dict);
    let max_d = params.max_distance.unwrap_or(2) as usize;
    let limit = params.limit.unwrap_or(20) as usize;

    let mut hits: Vec<(String, usize, f64)> = xducer
        .query_with_distance(&params.query, max_d)
        .map(|c| {
            let art = articulatory_distance_score(&params.query, &c.term);
            (c.term, c.distance, art)
        })
        .collect();
    hits.sort_by(|a, b| {
        a.1.cmp(&b.1)
            .then_with(|| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal))
    });
    hits.truncate(limit);

    json_result(&json!({
        "query": params.query,
        "project": params.project,
        "max_distance": max_d,
        "hits": hits.into_iter().map(|(path, distance, articulatory_distance)| json!({
            "path": path,
            "distance": distance,
            "articulatory_distance": articulatory_distance,
        })).collect::<Vec<_>>(),
    }))
}
