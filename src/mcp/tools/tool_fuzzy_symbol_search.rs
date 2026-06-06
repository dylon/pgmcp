//! `tool_fuzzy_symbol_search` (Phase 8, P14.3 — persistent
//! `FuzzyIndex` is the sole candidate source).
//!
//! Routes per-call queries through the on-disk
//! `PersistentARTrieChar`-backed `FuzzyIndex<SymbolValue>`
//! materialized by `cron::fuzzy_sync`. The trie persists across
//! daemon restarts; the helper `open_symbol_trie` lazy-warms it
//! from PG on first call (idempotent — safe to race the cron's
//! periodic rebuild).
//!
//! `DynamicDawgChar` is intentionally NOT used here: rebuilding
//! an in-memory DAWG from a PG `SELECT` on every MCP call wastes
//! O(n·log n) per request and discards everything between calls.
//! The right pick for an index that should survive restarts is
//! `PersistentARTrieChar`. (Per CLAUDE.md, `DynamicDawgChar` is
//! still appropriate for session-scoped / per-query / one-shot
//! ad-hoc vocabularies — but not here.)

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::fuzzy::limits::{bounded_limit, bounded_max_distance};
use crate::fuzzy::phonetic::articulatory_distance_score;
use crate::fuzzy::sync::open_symbol_trie;
use crate::mcp::server::FuzzySymbolSearchParams;
use crate::mcp::tools::sota_helpers::json_result;

pub async fn run(
    ctx: &SystemContext,
    params: FuzzySymbolSearchParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);

    let idx = open_symbol_trie(ctx, &params.project).await?;
    let max_d = bounded_max_distance(params.max_distance);
    let limit = bounded_limit(params.limit);
    let phonetic = params.phonetic.unwrap_or(false);

    // Edit mode (default): the persistent trie returns (term, distance, value)
    // via a Damerau-Levenshtein transducer. Phonetic mode: a composed
    // phonetic∘edit search over the same vocabulary (query+terms normalized by
    // the project's phonetic rules, matched in normalized space). Both apply an
    // articulatory re-rank tiebreaker so phonetically similar matches surface
    // above arbitrary substitutions at the same edit distance.
    let mut hits: Vec<(String, usize, f64)> = if phonetic {
        let terms = idx.iter_strings();
        let phon = ctx.phonetics_for(Some(&params.project));
        phon.phonetic_search(terms.iter(), &params.query, max_d)
            .into_iter()
            .map(|(term, distance, _normalized)| {
                let art = articulatory_distance_score(&params.query, &term);
                (term, distance, art)
            })
            .collect()
    } else {
        idx.query(&params.query, max_d)
            .into_iter()
            .map(|(term, distance, _value)| {
                let art = articulatory_distance_score(&params.query, &term);
                (term, distance, art)
            })
            .collect()
    };
    hits.sort_by(|a, b| {
        a.1.cmp(&b.1)
            .then_with(|| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal))
    });
    hits.truncate(limit);

    json_result(&json!({
        "query": params.query,
        "project": params.project,
        "max_distance": max_d,
        "mode": if phonetic { "phonetic" } else { "edit" },
        "vocabulary_size": idx.len(),
        "hits": hits.into_iter().map(|(term, distance, articulatory_distance)| json!({
            "term": term,
            "distance": distance,
            "articulatory_distance": articulatory_distance,
        })).collect::<Vec<_>>(),
    }))
}
