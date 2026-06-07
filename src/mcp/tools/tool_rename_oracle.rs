//! `tool_rename_oracle` (Phase 8) — picks the most-likely current-day
//! name for a removed symbol using Damerau-Levenshtein + articulatory
//! distance tiebreak (same pattern as tool_semver_break_audit).
use std::collections::BTreeSet;
use std::sync::atomic::Ordering;

use libdictenstein::dynamic_dawg_char::DynamicDawgChar;
use liblevenshtein::transducer::Transducer;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::fuzzy::phonetic::articulatory_distance_score_weighted;
use crate::mcp::server::RenameOracleParams;
use crate::mcp::tools::sota_helpers::json_result;

const RENAME_ORACLE_MAX_DISTANCE: usize = 2;
const RENAME_ORACLE_MAX_CANDIDATES: usize = 5_000;
const RENAME_ORACLE_MAX_NAME_BYTES: usize = 256;
const RENAME_ORACLE_MAX_TOTAL_NAME_BYTES: usize = 1_048_576;

pub async fn run(
    ctx: &SystemContext,
    params: RenameOracleParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);

    let removed_name = normalize_symbol_name("removed_name", &params.removed_name)?;
    if params.current_names.len() > RENAME_ORACLE_MAX_CANDIDATES {
        return Err(McpError::invalid_params(
            format!("current_names must contain at most {RENAME_ORACLE_MAX_CANDIDATES} candidates"),
            None,
        ));
    }

    let mut total_bytes = removed_name.len();
    let mut unique = BTreeSet::new();
    for current_name in &params.current_names {
        let current_name = normalize_symbol_name("current_names entries", current_name)?;
        total_bytes = total_bytes
            .checked_add(current_name.len())
            .filter(|bytes| *bytes <= RENAME_ORACLE_MAX_TOTAL_NAME_BYTES)
            .ok_or_else(|| {
                McpError::invalid_params(
                    format!(
                        "current_names total size must be at most {RENAME_ORACLE_MAX_TOTAL_NAME_BYTES} bytes"
                    ),
                    None,
                )
            })?;
        unique.insert(current_name);
    }

    if unique.is_empty() {
        return json_result(&json!({
            "removed_name": removed_name,
            "likely_rename_to": null,
            "candidate_count": 0,
            "max_distance": RENAME_ORACLE_MAX_DISTANCE,
        }));
    }

    let normalized_current_names: Vec<String> = unique.into_iter().collect();
    let names: Vec<&str> = normalized_current_names
        .iter()
        .map(String::as_str)
        .collect();
    let dict: DynamicDawgChar<()> = DynamicDawgChar::from_terms(names);
    let xducer = Transducer::with_transposition(dict);
    let weights = ctx.config().load().fuzzy.articulatory_weights();
    let best = xducer
        .query_with_distance(&removed_name, RENAME_ORACLE_MAX_DISTANCE)
        .min_by(|a, b| {
            let aad = articulatory_distance_score_weighted(&removed_name, &a.term, &weights);
            let bad = articulatory_distance_score_weighted(&removed_name, &b.term, &weights);
            aad.partial_cmp(&bad)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.distance.cmp(&b.distance))
        });
    let likely_rename = best.map(|c| c.term);
    json_result(&json!({
        "removed_name": removed_name,
        "likely_rename_to": likely_rename,
        "candidate_count": normalized_current_names.len(),
        "max_distance": RENAME_ORACLE_MAX_DISTANCE,
    }))
}

fn normalize_symbol_name(field: &str, raw: &str) -> Result<String, McpError> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(McpError::invalid_params(
            format!("{field} must be non-empty"),
            None,
        ));
    }
    if name.len() > RENAME_ORACLE_MAX_NAME_BYTES {
        return Err(McpError::invalid_params(
            format!("{field} must be at most {RENAME_ORACLE_MAX_NAME_BYTES} bytes"),
            None,
        ));
    }
    Ok(name.to_string())
}
