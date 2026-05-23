//! `tool_expand_query_to_phonetic_pattern` (Phase 8).
use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::server::ExpandQueryToPhoneticPatternParams;
use crate::mcp::tools::sota_helpers::json_result;

pub async fn run(
    ctx: &SystemContext,
    params: ExpandQueryToPhoneticPatternParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    // Without a loaded RuleSet, "expansion" reduces to the identity
    // alternation. The framework returns the input unchanged; when
    // PgmcpPhonetics (Phase 10) loads .pgmcp/rules.llev, this same
    // tool produces the full reverse-expansion regex.
    json_result(&json!({
        "input": params.term,
        "expanded": params.term.clone(),
        "guidance": "Full reverse-expansion (e.g. `nite` → `(n|kn)i(t|te|ght)`) requires \
                     a loaded RuleSet via PgmcpPhonetics. This tool returns the framework \
                     hook with the identity expansion in the no-rule case."
    }))
}
