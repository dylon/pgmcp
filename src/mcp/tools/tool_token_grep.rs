//! `tool_token_grep` (Phase 8).
use std::sync::atomic::Ordering;

use libdictenstein::dynamic_dawg_char::DynamicDawgChar;
use liblevenshtein::transducer::Transducer;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::server::TokenGrepParams;
use crate::mcp::tools::sota_helpers::json_result;

pub async fn run(ctx: &SystemContext, params: TokenGrepParams) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let tokens: Vec<&str> = params.haystack.iter().map(|s| s.as_str()).collect();
    let dict: DynamicDawgChar<()> = DynamicDawgChar::from_terms(tokens);
    let xducer = Transducer::with_transposition(dict);
    let max_d = params.max_distance.unwrap_or(2) as usize;
    let hits: Vec<(String, usize)> = xducer
        .query_with_distance(&params.query, max_d)
        .map(|c| (c.term, c.distance))
        .collect();
    json_result(&json!({
        "query": params.query,
        "max_distance": max_d,
        "matches": hits.into_iter().map(|(t, d)| json!({"token": t, "distance": d}))
            .collect::<Vec<_>>(),
    }))
}
