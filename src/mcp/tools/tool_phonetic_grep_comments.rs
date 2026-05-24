//! `tool_phonetic_grep_comments` (Phase 8, P13.4 real implementation).
//!
//! Uses liblevenshtein's `PhoneticGrepOnline` streaming scanner over
//! the supplied haystack lines. For each line, the scanner runs the
//! phonetic-normalized query against the document; matches return
//! `byte_range`, `original_text`, `normalized_text`, and `distance`.
//!
//! P13.4 changes from the prior stub: previously the tool scored
//! each haystack line by raw articulatory distance, which produced
//! line-level rankings rather than character-level matches. The
//! real implementation surfaces character-anchored matches with the
//! position data the framework gives us.

use std::sync::atomic::Ordering;

use liblevenshtein::phonetic::grep_online::PhoneticGrepOnline;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::fuzzy::phonetic::PgmcpPhonetics;
use crate::mcp::server::PhoneticGrepCommentsParams;
use crate::mcp::tools::sota_helpers::json_result;

pub async fn run(
    ctx: &SystemContext,
    params: PhoneticGrepCommentsParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);

    let phon = PgmcpPhonetics::default_english();
    let rules = phon.rules();
    // Default to distance 1: tolerates a single character drift on
    // top of phonetic normalization. Caller-tunable via the future
    // max_distance param landing in P13.4 follow-up.
    let max_distance: u8 = 1;
    let scanner = PhoneticGrepOnline::with_rules(&params.query, (*rules).clone(), max_distance)
        .case_insensitive(true);

    // Scan each haystack line. PhoneticGrepOnline owns the
    // single-document scan path; for a Vec<String> we run it
    // per-line and emit one record per match with the originating
    // line index.
    let mut matches: Vec<serde_json::Value> = Vec::new();
    for (line_index, line) in params.haystack.iter().enumerate() {
        for m in scanner.scan(line) {
            matches.push(json!({
                "line_index": line_index,
                "line": line,
                "byte_start": m.byte_range.0,
                "byte_end": m.byte_range.1,
                "char_start": m.char_range.0,
                "char_end": m.char_range.1,
                "original_text": m.original_text,
                "normalized_text": m.normalized_text,
                "distance": m.distance,
            }));
        }
    }

    // Sort by distance (ascending), then line_index, then byte_start.
    matches.sort_by(|a, b| {
        let da = a["distance"].as_u64().unwrap_or(u64::MAX);
        let db = b["distance"].as_u64().unwrap_or(u64::MAX);
        da.cmp(&db)
            .then_with(|| {
                let la = a["line_index"].as_u64().unwrap_or(0);
                let lb = b["line_index"].as_u64().unwrap_or(0);
                la.cmp(&lb)
            })
            .then_with(|| {
                let ba = a["byte_start"].as_u64().unwrap_or(0);
                let bb = b["byte_start"].as_u64().unwrap_or(0);
                ba.cmp(&bb)
            })
    });

    json_result(&json!({
        "query": params.query,
        "max_distance": max_distance,
        "language": phon.language().as_str(),
        "match_count": matches.len(),
        "matches": matches,
    }))
}
