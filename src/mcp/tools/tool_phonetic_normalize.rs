//! `tool_phonetic_normalize` (Phase 8).
use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::server::PhoneticNormalizeParams;
use crate::mcp::tools::sota_helpers::json_result;

pub async fn run(
    ctx: &SystemContext,
    params: PhoneticNormalizeParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    // The framework's full normalization needs a loaded RuleSet; for the
    // representative tool surface we return the term's articulatory
    // self-distance (always 0) as a smoke + the input echoed, which the
    // PgmcpPhonetics layer (Phase 10) will extend with full rule-based
    // normalization when the .pgmcp/rules.llev loader lands.
    json_result(&json!({
        "input": params.term,
        "articulatory_self_distance": crate::fuzzy::phonetic::articulatory_distance_score(
            &params.term, &params.term),
        "guidance": "Self-distance is always 0.0. Full rule-based normalization (loading \
                     .pgmcp/rules.llev per-project) wires through PgmcpPhonetics; \
                     this MCP tool surfaces the framework hook."
    }))
}
