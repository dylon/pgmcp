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

pub async fn run(ctx: &SystemContext, params: FuzzyGrepParams) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let max_d = params.max_distance.unwrap_or(2) as u8;
    let grep = TokenGrep::new(&params.query, max_d)
        .map_err(|e| McpError::internal_error(format!("fuzzy grep query: {e:?}"), None))?;

    let mut matches = Vec::new();
    for (index, doc) in params.haystack.iter().enumerate() {
        for m in grep.scan(doc) {
            matches.push(json!({
                "haystack_index": index,
                "matched_text": m.matched_text,
                "byte_start": m.byte_range.0,
                "byte_end": m.byte_range.1,
                "distance": m.total_distance,
            }));
        }
    }

    json_result(&json!({
        "query": params.query,
        "max_distance": max_d,
        "match_count": matches.len(),
        "matches": matches,
    }))
}
