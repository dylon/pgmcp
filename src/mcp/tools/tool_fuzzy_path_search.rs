//! `tool_fuzzy_path_search` (Phase 8, P14.3 — persistent
//! `FuzzyIndex` is the sole candidate source).
//!
//! Mirror of `tool_fuzzy_symbol_search` for the per-project path
//! trie (`indexed_files.relative_path`). Same rationale: the
//! persistent `FuzzyIndex<PathValue>` is the right backend for an
//! index that should survive daemon restarts, and the lazy-warm
//! on first call obviates the cron-must-have-run-first concern.

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::fuzzy::limits::{bounded_limit, bounded_max_distance};
use crate::fuzzy::phonetic::articulatory_distance_score;
use crate::fuzzy::sync::open_path_trie;
use crate::mcp::server::FuzzyPathSearchParams;
use crate::mcp::tools::sota_helpers::json_result;

pub async fn run(
    ctx: &SystemContext,
    params: FuzzyPathSearchParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);

    let idx = open_path_trie(ctx, &params.project).await?;
    let max_d = bounded_max_distance(params.max_distance);
    let limit = bounded_limit(params.limit);

    let mut hits: Vec<(String, usize, f64)> = idx
        .query(&params.query, max_d)
        .into_iter()
        .map(|(path, distance, _value)| {
            let art = articulatory_distance_score(&params.query, &path);
            (path, distance, art)
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
        "vocabulary_size": idx.len(),
        "hits": hits.into_iter().map(|(path, distance, articulatory_distance)| json!({
            "path": path,
            "distance": distance,
            "articulatory_distance": articulatory_distance,
        })).collect::<Vec<_>>(),
    }))
}
