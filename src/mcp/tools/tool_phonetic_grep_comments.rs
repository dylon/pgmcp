//! `tool_phonetic_grep_comments` (Phase 8).
//!
//! Phonetic-fuzzy grep over short text lines. Without a loaded
//! .pgmcp/rules.llev RuleSet the framework's phonetic normalization
//! reduces to the articulatory-distance heuristic, which is what this
//! tool surfaces today. The full streaming `PhoneticGrepOnline`
//! pipeline lands alongside PgmcpPhonetics.

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::fuzzy::phonetic::articulatory_distance_score;
use crate::mcp::server::PhoneticGrepCommentsParams;
use crate::mcp::tools::sota_helpers::json_result;

pub async fn run(
    ctx: &SystemContext,
    params: PhoneticGrepCommentsParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let mut scored: Vec<(String, f64)> = params
        .haystack
        .iter()
        .map(|line| {
            (
                line.clone(),
                articulatory_distance_score(&params.query, line),
            )
        })
        .collect();
    scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(20);
    json_result(&json!({
        "query": params.query,
        "matches": scored.into_iter().map(|(l, d)| json!({"line": l, "articulatory_distance": d}))
            .collect::<Vec<_>>(),
    }))
}
