//! `tool_substring_search` (Phase 8).
use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::fuzzy::suffix_automaton::SubstringIndex;
use crate::mcp::server::SubstringSearchParams;
use crate::mcp::tools::sota_helpers::json_result;

pub async fn run(
    ctx: &SystemContext,
    params: SubstringSearchParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let idx = SubstringIndex::from_terms(params.haystack.iter());
    let found = idx.contains_substring(&params.needle);
    json_result(&json!({
        "needle": params.needle,
        "haystack_size": idx.len(),
        "contains_substring": found,
    }))
}
