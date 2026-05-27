//! `tool_phonetic_naming_consistency` (Phase 8).
use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::fuzzy::phonetic::articulatory_distance_score_weighted;
use crate::mcp::server::PhoneticNamingConsistencyParams;
use crate::mcp::tools::sota_helpers::json_result;

/// Estimate an identifier's syllable count via a vowel-group heuristic: split
/// the identifier into word tokens (snake_case / camelCase / digit boundaries)
/// and count maximal runs of vowels in each token. Adequate for flagging
/// gross syllable-count drift between near-synonymous names; not a linguistic
/// syllabifier.
fn estimate_syllables(identifier: &str) -> u32 {
    fn is_vowel(c: char) -> bool {
        matches!(c.to_ascii_lowercase(), 'a' | 'e' | 'i' | 'o' | 'u' | 'y')
    }
    let mut syllables = 0u32;
    let mut prev_vowel = false;
    let mut prev_lower = false;
    for c in identifier.chars() {
        // Token boundary (camelCase hump, separator, digit): reset vowel run.
        let boundary =
            c == '_' || c == '-' || c.is_ascii_digit() || (c.is_uppercase() && prev_lower);
        if boundary {
            prev_vowel = false;
        }
        let vowel = c.is_alphabetic() && is_vowel(c);
        if vowel && !prev_vowel {
            syllables += 1;
        }
        prev_vowel = vowel;
        prev_lower = c.is_lowercase();
    }
    syllables
}

pub async fn run(
    ctx: &SystemContext,
    params: PhoneticNamingConsistencyParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let cfg = ctx.config().load();
    // Threshold is configurable; defaults to the shared [fuzzy].phonetic_merge_threshold
    // so this tool, tool_find_duplicates, and tool_find_similar_modules agree on what
    // "phonetically similar" means.
    let threshold = params
        .max_distance
        .unwrap_or(cfg.fuzzy.phonetic_merge_threshold);
    // Per-dimension articulatory weights from the [fuzzy] knobs.
    let weights = cfg.fuzzy.articulatory_weights();
    let syllable_drift_max = cfg.fuzzy.syllable_drift_max_delta;

    let n = params.identifiers.len();
    let mut flags: Vec<serde_json::Value> = Vec::new();
    let mut syllable_drift_pairs: Vec<serde_json::Value> = Vec::new();
    for i in 0..n {
        for j in (i + 1)..n {
            let a = &params.identifiers[i];
            let b = &params.identifiers[j];
            let d = articulatory_distance_score_weighted(a, b, &weights);
            // Identifiers within `threshold` articulatory distance and >2 chars
            // are "phonetically similar".
            if d > 0.0 && d <= threshold && a.len() > 2 && b.len() > 2 {
                flags.push(json!({
                    "a": a, "b": b, "articulatory_distance": d
                }));
                // Among phonetically-similar names, flag any whose syllable
                // counts diverge by more than the configured delta — a sign of
                // an inconsistent rename (e.g. `config` vs `configuration`).
                let (sa, sb) = (estimate_syllables(a), estimate_syllables(b));
                if sa.abs_diff(sb) > syllable_drift_max {
                    syllable_drift_pairs.push(json!({
                        "a": a, "b": b, "syllables_a": sa, "syllables_b": sb
                    }));
                }
            }
        }
    }
    json_result(&json!({
        "n_identifiers": n,
        "threshold": threshold,
        "syllable_drift_max_delta": syllable_drift_max,
        "phonetically_similar_pairs": flags,
        "syllable_drift_pairs": syllable_drift_pairs,
    }))
}
