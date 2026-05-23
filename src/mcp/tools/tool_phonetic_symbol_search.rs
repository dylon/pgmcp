//! `tool_phonetic_symbol_search` (Phase 8).
use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::fuzzy::phonetic::articulatory_distance_score;
use crate::mcp::server::PhoneticSymbolSearchParams;
use crate::mcp::tools::sota_helpers::json_result;

pub async fn run(
    ctx: &SystemContext,
    params: PhoneticSymbolSearchParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let mut scored: Vec<(String, f64)> = params
        .candidates
        .iter()
        .map(|c| (c.clone(), articulatory_distance_score(&params.query, c)))
        .collect();
    scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(20);
    json_result(&json!({
        "query": params.query,
        "matches": scored.into_iter().map(|(c, d)| json!({"symbol": c, "articulatory_distance": d}))
            .collect::<Vec<_>>(),
    }))
}
