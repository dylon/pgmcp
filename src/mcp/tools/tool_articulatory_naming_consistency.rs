//! `tool_articulatory_naming_consistency` (Phase 8).
use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::fuzzy::phonetic::articulatory_distance_score;
use crate::mcp::server::ArticulatoryNamingConsistencyParams;
use crate::mcp::tools::sota_helpers::json_result;

pub async fn run(
    ctx: &SystemContext,
    params: ArticulatoryNamingConsistencyParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let threshold = params.max_distance.unwrap_or(0.5);
    let n = params.identifiers.len();
    let mut close: Vec<serde_json::Value> = Vec::new();
    for i in 0..n {
        for j in (i + 1)..n {
            let d = articulatory_distance_score(&params.identifiers[i], &params.identifiers[j]);
            if d > 0.0 && d <= threshold {
                close.push(json!({
                    "a": params.identifiers[i].clone(),
                    "b": params.identifiers[j].clone(),
                    "articulatory_distance": d,
                }));
            }
        }
    }
    json_result(&json!({
        "n_identifiers": n,
        "threshold": threshold,
        "articulatory_clusters": close,
    }))
}
