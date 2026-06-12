//! `tool_mandate_dedup_v2` (Phase 8).
use std::collections::HashMap;
use std::sync::atomic::Ordering;

use libdictenstein::dynamic_dawg::char::DynamicDawgChar;
use liblevenshtein::transducer::Transducer;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::server::MandateDedupV2Params;
use crate::mcp::tools::sota_helpers::json_result;

pub async fn run(
    ctx: &SystemContext,
    params: MandateDedupV2Params,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let max_d = params.max_distance.unwrap_or(3) as usize;
    let mut id_index: HashMap<String, Vec<i64>> = HashMap::new();
    for entry in &params.active {
        id_index
            .entry(entry.imperative.to_lowercase())
            .or_default()
            .push(entry.id);
    }
    let terms: Vec<&str> = id_index.keys().map(|s| s.as_str()).collect();
    let dict: DynamicDawgChar<()> = DynamicDawgChar::from_terms(terms);
    let xducer = Transducer::with_transposition(dict);

    let new_lower = params.new_imperative.to_lowercase();
    let mut ids: Vec<i64> = Vec::new();
    for candidate in xducer.query_with_distance(&new_lower, max_d) {
        if let Some(matched) = id_index.get(&candidate.term) {
            ids.extend(matched.iter().copied());
        }
    }
    ids.sort();
    ids.dedup();
    json_result(&json!({
        "new_imperative": params.new_imperative,
        "max_distance": max_d,
        "superseded_ids": ids,
    }))
}
