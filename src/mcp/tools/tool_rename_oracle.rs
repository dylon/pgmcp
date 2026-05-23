//! `tool_rename_oracle` (Phase 8) — picks the most-likely current-day
//! name for a removed symbol using Damerau-Levenshtein + articulatory
//! distance tiebreak (same pattern as tool_semver_break_audit).
use std::sync::atomic::Ordering;

use libdictenstein::dynamic_dawg_char::DynamicDawgChar;
use liblevenshtein::transducer::Transducer;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::fuzzy::phonetic::articulatory_distance_score;
use crate::mcp::server::RenameOracleParams;
use crate::mcp::tools::sota_helpers::json_result;

pub async fn run(
    ctx: &SystemContext,
    params: RenameOracleParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let names: Vec<&str> = params.current_names.iter().map(|s| s.as_str()).collect();
    let dict: DynamicDawgChar<()> = DynamicDawgChar::from_terms(names);
    let xducer = Transducer::with_transposition(dict);
    let candidates: Vec<liblevenshtein::transducer::Candidate> = xducer
        .query_with_distance(&params.removed_name, 2)
        .collect();
    let best = candidates.into_iter().min_by(|a, b| {
        let aad = articulatory_distance_score(&params.removed_name, &a.term);
        let bad = articulatory_distance_score(&params.removed_name, &b.term);
        aad.partial_cmp(&bad)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.distance.cmp(&b.distance))
    });
    let likely_rename = best.map(|c| c.term);
    json_result(&json!({
        "removed_name": params.removed_name,
        "likely_rename_to": likely_rename,
    }))
}
