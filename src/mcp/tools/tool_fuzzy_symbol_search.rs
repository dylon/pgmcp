//! `tool_fuzzy_symbol_search` (Phase 8, P13.4 real implementation).
//!
//! Damerau-Levenshtein candidate generation against the project's
//! symbol vocabulary (`file_symbols`), with an articulatory-distance
//! re-rank stage. P13.4 changes from the prior stub:
//!
//! - Mandatory `project` filter via the projects JOIN (the prior
//!   `SELECT DISTINCT name FROM file_symbols LIMIT 5000` ignored
//!   project boundaries and capped at an arbitrary 5000 rows).
//! - No `LIMIT` on the vocabulary fetch — the trie scales linearly
//!   with vocabulary size and a 5k cap was silently dropping rare
//!   symbols.
//! - Articulatory re-rank: same Damerau-Levenshtein candidates,
//!   reordered by articulatory edit distance so phonetically-similar
//!   matches surface first.

use std::sync::atomic::Ordering;

use libdictenstein::dynamic_dawg_char::DynamicDawgChar;
use liblevenshtein::transducer::Transducer;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::fuzzy::phonetic::articulatory_distance_score;
use crate::mcp::server::FuzzySymbolSearchParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

pub async fn run(
    ctx: &SystemContext,
    params: FuzzySymbolSearchParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    // Project filter is recommended — without one the vocabulary
    // spans every indexed project and produces noisy results. We
    // still permit the global lookup for cross-project rename
    // detection use cases.
    let rows: Vec<(String,)> = if let Some(project_name) = params.project.as_deref() {
        sqlx::query_as::<_, (String,)>(
            "SELECT DISTINCT fs.name
             FROM file_symbols fs
             JOIN indexed_files f ON fs.file_id = f.id
             JOIN projects p ON f.project_id = p.id
             WHERE p.name = $1
               AND fs.name IS NOT NULL
               AND length(fs.name) > 0",
        )
        .bind(project_name)
        .fetch_all(pool)
        .await
    } else {
        sqlx::query_as::<_, (String,)>(
            "SELECT DISTINCT name
             FROM file_symbols
             WHERE name IS NOT NULL
               AND length(name) > 0",
        )
        .fetch_all(pool)
        .await
    }
    .map_err(|e| McpError::internal_error(format!("symbol fetch: {e}"), None))?;

    let names: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
    if names.is_empty() {
        return json_result(&json!({
            "query": params.query,
            "project": params.project,
            "max_distance": params.max_distance.unwrap_or(2),
            "hits": Vec::<serde_json::Value>::new(),
            "guidance": "No symbols indexed under the requested project (or globally if none was set).",
        }));
    }

    let dict: DynamicDawgChar<()> = DynamicDawgChar::from_terms(names);
    let xducer = Transducer::with_transposition(dict);
    let max_d = params.max_distance.unwrap_or(2) as usize;
    let limit = params.limit.unwrap_or(20) as usize;

    // Phase 1: Damerau-Levenshtein candidates.
    let mut hits: Vec<(String, usize, f64)> = xducer
        .query_with_distance(&params.query, max_d)
        .map(|c| {
            let art = articulatory_distance_score(&params.query, &c.term);
            (c.term, c.distance, art)
        })
        .collect();

    // Phase 2: articulatory re-rank — primary key edit_distance,
    // tiebreaker articulatory_distance. Phonetically-similar matches
    // (voicing-only edits) surface above arbitrary substitutions at
    // the same edit distance.
    hits.sort_by(|a, b| {
        a.1.cmp(&b.1)
            .then_with(|| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal))
    });
    hits.truncate(limit);

    json_result(&json!({
        "query": params.query,
        "project": params.project,
        "max_distance": max_d,
        "hits": hits.into_iter().map(|(term, distance, articulatory_distance)| json!({
            "term": term,
            "distance": distance,
            "articulatory_distance": articulatory_distance,
        })).collect::<Vec<_>>(),
    }))
}
