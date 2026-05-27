//! `tool_phonetic_symbol_search` — composed phonetic∘edit symbol search.
//!
//! Builds a transient `PhoneticNormalizedDictionary` over the project's
//! persistent symbol-trie vocabulary (lazy-warmed from PG on first use),
//! then matches the query in phonetic-normalized space within the given edit
//! distance. Results carry the symbol's `SymbolValue` payload (kind,
//! visibility, file_id, line) and are ranked by edit distance with an
//! articulatory-distance tiebreaker. Unlike the previous implementation, this
//! actually searches the index — the caller no longer supplies candidates.
use std::collections::HashMap;
use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::fuzzy::phonetic::articulatory_distance_score;
use crate::fuzzy::sync::open_symbol_trie;
use crate::fuzzy::values::SymbolValue;
use crate::mcp::server::PhoneticSymbolSearchParams;
use crate::mcp::tools::sota_helpers::json_result;

pub async fn run(
    ctx: &SystemContext,
    params: PhoneticSymbolSearchParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let max_d = params.max_distance.unwrap_or(2) as usize;
    let limit = params.limit.unwrap_or(20) as usize;

    // Consult the per-project persistent symbol trie (lazy-warmed from PG on
    // first call; kept current by the fuzzy-sync cron thereafter).
    let idx = open_symbol_trie(ctx, &params.project).await?;
    let vocab = idx.iter_with_values();
    let value_by_term: HashMap<String, SymbolValue> = vocab.iter().cloned().collect();
    let terms: Vec<String> = vocab.into_iter().map(|(term, _)| term).collect();

    let phon = ctx.phonetics_for(Some(&params.project));
    let mut hits: Vec<(String, usize, String, f64)> = phon
        .phonetic_search(terms.iter(), &params.query, max_d)
        .into_iter()
        .map(|(term, distance, normalized)| {
            let art = articulatory_distance_score(&params.query, &term);
            (term, distance, normalized, art)
        })
        .collect();
    // Primary: edit distance in normalized space; tiebreak: articulatory distance.
    hits.sort_by(|a, b| {
        a.1.cmp(&b.1)
            .then(a.3.partial_cmp(&b.3).unwrap_or(std::cmp::Ordering::Equal))
    });
    hits.truncate(limit);

    json_result(&json!({
        "query": params.query,
        "project": params.project,
        "max_distance": max_d,
        "matches": hits
            .into_iter()
            .map(|(term, distance, normalized, art)| {
                let value = value_by_term.get(&term);
                json!({
                    "symbol": term,
                    "distance": distance,
                    "normalized_form": normalized,
                    "articulatory_distance": art,
                    "kind": value.map(|v| v.kind.clone()),
                    "visibility": value.map(|v| v.visibility.clone()),
                    "file_id": value.map(|v| v.file_id),
                    "line": value.map(|v| v.line),
                })
            })
            .collect::<Vec<_>>(),
    }))
}
