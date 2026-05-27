//! `tool_token_grep` — structured token-pattern fuzzy grep over a
//! caller-supplied haystack via liblevenshtein's `TokenGrep`.
//!
//! The query uses `TokenGrep`'s token-query grammar (whitespace/`.*`-separated
//! tokens, `(a|b)` alternations, `"phrases"`, per-token `:distance`). Each
//! haystack entry is scanned as a document; matches carry byte spans plus
//! per-token detail (original/normalized text + per-token distance). Replaces
//! the prior transient-`DynamicDawgChar`, whole-string approach.
use std::sync::atomic::Ordering;

use liblevenshtein::phonetic::token_grep::TokenGrep;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::server::TokenGrepParams;
use crate::mcp::tools::sota_helpers::json_result;

pub async fn run(ctx: &SystemContext, params: TokenGrepParams) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let max_d = params.max_distance.unwrap_or(2) as u8;
    let grep = TokenGrep::new(&params.query, max_d)
        .map_err(|e| McpError::internal_error(format!("token grep query: {e:?}"), None))?;

    let mut matches = Vec::new();
    for (index, doc) in params.haystack.iter().enumerate() {
        for m in grep.scan(doc) {
            matches.push(json!({
                "haystack_index": index,
                "matched_text": m.matched_text,
                "byte_start": m.byte_range.0,
                "byte_end": m.byte_range.1,
                "total_distance": m.total_distance,
                "tokens": m
                    .token_matches
                    .iter()
                    .map(|t| json!({
                        "token_index": t.token_index,
                        "original_text": t.original_text,
                        "normalized_text": t.normalized_text,
                        "distance": t.distance,
                    }))
                    .collect::<Vec<_>>(),
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
