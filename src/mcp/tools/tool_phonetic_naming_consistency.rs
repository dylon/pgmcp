//! `tool_phonetic_naming_consistency` (Phase 8).
use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::fuzzy::phonetic::articulatory_distance_score;
use crate::mcp::server::PhoneticNamingConsistencyParams;
use crate::mcp::tools::sota_helpers::json_result;

pub async fn run(
    ctx: &SystemContext,
    params: PhoneticNamingConsistencyParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let n = params.identifiers.len();
    let mut flags: Vec<serde_json::Value> = Vec::new();
    for i in 0..n {
        for j in (i + 1)..n {
            let a = &params.identifiers[i];
            let b = &params.identifiers[j];
            let d = articulatory_distance_score(a, b);
            // Heuristic threshold: identifiers within 1.5 articulatory
            // distance and >2 chars are "phonetically similar".
            if d > 0.0 && d <= 1.5 && a.len() > 2 && b.len() > 2 {
                flags.push(json!({
                    "a": a, "b": b, "articulatory_distance": d
                }));
            }
        }
    }
    json_result(&json!({
        "n_identifiers": n,
        "phonetically_similar_pairs": flags,
    }))
}
