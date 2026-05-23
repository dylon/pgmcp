//! `tool_fuzzy_symbol_search` (Phase 8) — in-process Transducer query.
//!
//! The persistent on-disk FuzzyIndex (Phase 4) is opened and queried
//! by the daemon's fuzzy-sync cron path; this MCP tool runs an
//! in-process Damerau-Levenshtein query against the symbol set the
//! caller provides so it can be exercised without a populated trie.

use std::sync::atomic::Ordering;

use libdictenstein::dynamic_dawg_char::DynamicDawgChar;
use liblevenshtein::transducer::Transducer;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::server::FuzzySymbolSearchParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

pub async fn run(
    ctx: &SystemContext,
    params: FuzzySymbolSearchParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    // Pull current symbol names from file_symbols (Shadow-ASR).
    let rows: Vec<(String,)> =
        sqlx::query_as::<_, (String,)>("SELECT DISTINCT name FROM file_symbols LIMIT 5000")
            .fetch_all(pool)
            .await
            .map_err(|e| McpError::internal_error(format!("symbol fetch: {e}"), None))?;

    let names: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
    let dict: DynamicDawgChar<()> = DynamicDawgChar::from_terms(names);
    let xducer = Transducer::with_transposition(dict);
    let max_d = params.max_distance.unwrap_or(2) as usize;
    let limit = params.limit.unwrap_or(20) as usize;
    let mut hits: Vec<(String, usize)> = xducer
        .query_with_distance(&params.query, max_d)
        .map(|c| (c.term, c.distance))
        .collect();
    hits.sort_by_key(|(_, d)| *d);
    hits.truncate(limit);
    json_result(&json!({
        "query": params.query,
        "max_distance": max_d,
        "hits": hits.into_iter().map(|(t, d)| json!({"term": t, "distance": d}))
            .collect::<Vec<_>>(),
    }))
}
