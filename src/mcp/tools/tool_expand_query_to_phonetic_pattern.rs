//! `tool_expand_query_to_phonetic_pattern` (Phase 8, P13.4 real
//! implementation).
//!
//! Returns the regex-alternation pattern that matches phonetic
//! variants of `term` under the embedded English rule pack. For
//! example, `"nite"` may expand to `(n|kn)i(t|te|ght)` depending
//! on the loaded rule set's coverage.

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::fuzzy::phonetic::PgmcpPhonetics;
use crate::mcp::server::ExpandQueryToPhoneticPatternParams;
use crate::mcp::tools::sota_helpers::json_result;

pub async fn run(
    ctx: &SystemContext,
    params: ExpandQueryToPhoneticPatternParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let phon = PgmcpPhonetics::default_english();
    let expanded = phon.expand_to_pattern(&params.term);
    json_result(&json!({
        "input": params.term,
        "expanded": expanded,
        "language": phon.language().as_str(),
    }))
}
