//! `tool_fuzzy_path_search` (Phase 8).
use std::sync::atomic::Ordering;

use libdictenstein::dynamic_dawg_char::DynamicDawgChar;
use liblevenshtein::transducer::Transducer;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::server::FuzzyPathSearchParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

pub async fn run(
    ctx: &SystemContext,
    params: FuzzyPathSearchParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let rows: Vec<(String,)> =
        sqlx::query_as::<_, (String,)>("SELECT relative_path FROM indexed_files LIMIT 5000")
            .fetch_all(pool)
            .await
            .map_err(|e| McpError::internal_error(format!("path fetch: {e}"), None))?;
    let paths: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
    let dict: DynamicDawgChar<()> = DynamicDawgChar::from_terms(paths);
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
        "hits": hits.into_iter().map(|(t, d)| json!({"path": t, "distance": d}))
            .collect::<Vec<_>>(),
    }))
}
