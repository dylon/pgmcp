//! `tool_fuzzy_grep` — positional fuzzy grep over a caller-supplied haystack
//! via liblevenshtein's `TokenGrep`.
//!
//! Each haystack entry is scanned as a document; the query is matched fuzzily
//! at every position (not as a whole-line dictionary term), so results carry
//! byte-offset spans + the matched text + edit distance. This replaces the
//! prior approach (rebuilding a transient `DynamicDawgChar` per call and
//! matching the query against entire haystack strings).
use std::sync::atomic::Ordering;

use liblevenshtein::phonetic::token_grep::TokenGrep;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::server::FuzzyGrepParams;
use crate::mcp::tools::sota_helpers::json_result;

const DEFAULT_FUZZY_GREP_DISTANCE: u32 = 2;
const MAX_FUZZY_GREP_DISTANCE: u32 = 8;
const MAX_FUZZY_GREP_QUERY_BYTES: usize = 512;
const MAX_FUZZY_GREP_DOCUMENTS: usize = 512;
const MAX_FUZZY_GREP_DOCUMENT_BYTES: usize = 64 * 1024;
const MAX_FUZZY_GREP_TOTAL_BYTES: usize = 1024 * 1024;
const MAX_FUZZY_GREP_MATCHES: usize = 1_000;

pub async fn run(ctx: &SystemContext, params: FuzzyGrepParams) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);

    let query = params.query.trim();
    if query.is_empty() {
        return Err(McpError::invalid_params("query must be non-empty", None));
    }
    if query.len() > MAX_FUZZY_GREP_QUERY_BYTES {
        return Err(McpError::invalid_params(
            format!("query must be at most {MAX_FUZZY_GREP_QUERY_BYTES} bytes"),
            None,
        ));
    }
    if params.haystack.len() > MAX_FUZZY_GREP_DOCUMENTS {
        return Err(McpError::invalid_params(
            format!("haystack must contain at most {MAX_FUZZY_GREP_DOCUMENTS} documents"),
            None,
        ));
    }

    let mut total_bytes = 0usize;
    for (index, doc) in params.haystack.iter().enumerate() {
        let doc_len = doc.len();
        if doc_len > MAX_FUZZY_GREP_DOCUMENT_BYTES {
            return Err(McpError::invalid_params(
                format!("haystack[{index}] must be at most {MAX_FUZZY_GREP_DOCUMENT_BYTES} bytes"),
                None,
            ));
        }
        total_bytes = total_bytes.saturating_add(doc_len);
        if total_bytes > MAX_FUZZY_GREP_TOTAL_BYTES {
            return Err(McpError::invalid_params(
                format!("haystack total size must be at most {MAX_FUZZY_GREP_TOTAL_BYTES} bytes"),
                None,
            ));
        }
    }

    validate_explicit_distance_budgets(query)?;
    let max_d = params
        .max_distance
        .unwrap_or(DEFAULT_FUZZY_GREP_DISTANCE)
        .min(MAX_FUZZY_GREP_DISTANCE) as u8;
    let grep = TokenGrep::new(query, max_d)
        .map_err(|e| McpError::internal_error(format!("fuzzy grep query: {e:?}"), None))?;

    let mut matches = Vec::new();
    let mut matches_truncated = false;
    for (index, doc) in params.haystack.iter().enumerate() {
        for m in grep.scan(doc) {
            if matches.len() >= MAX_FUZZY_GREP_MATCHES {
                matches_truncated = true;
                break;
            }
            matches.push(json!({
                "haystack_index": index,
                "matched_text": m.matched_text,
                "byte_start": m.byte_range.0,
                "byte_end": m.byte_range.1,
                "distance": m.total_distance,
            }));
        }
        if matches_truncated {
            break;
        }
    }

    json_result(&json!({
        "query": query,
        "max_distance": max_d,
        "match_count": matches.len(),
        "matches_truncated": matches_truncated,
        "reported_match_count": matches.len(),
        "matches": matches,
    }))
}

fn validate_explicit_distance_budgets(query: &str) -> Result<(), McpError> {
    let mut chars = query.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            let _ = chars.next();
            continue;
        }
        if ch != ':' || !chars.peek().is_some_and(|c| c.is_ascii_digit()) {
            continue;
        }

        let mut value = 0u32;
        while let Some(next) = chars.peek().copied() {
            let Some(digit) = next.to_digit(10) else {
                break;
            };
            let _ = chars.next();
            value = value.saturating_mul(10).saturating_add(digit);
            if value > MAX_FUZZY_GREP_DISTANCE {
                return Err(McpError::invalid_params(
                    format!("explicit token distance must be at most {MAX_FUZZY_GREP_DISTANCE}"),
                    None,
                ));
            }
        }
    }
    Ok(())
}
