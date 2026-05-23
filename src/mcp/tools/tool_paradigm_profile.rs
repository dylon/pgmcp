//! `tool_paradigm_profile` — wraps `code_analysis::paradigm` for MCP.
//!
//! Plan: `~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md`
//! Phase 8.

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::code_analysis::paradigm::analyze_code;
use crate::context::SystemContext;
use crate::mcp::server::ParadigmProfileParams;
use crate::mcp::tools::sota_helpers::json_result;

pub async fn tool_paradigm_profile(
    ctx: &SystemContext,
    params: ParadigmProfileParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let profile = analyze_code(&params.code);

    json_result(&json!({
        "oop_score": profile.oop_score,
        "fp_score": profile.fp_score,
        "reactive_score": profile.reactive_score,
        "procedural_score": profile.procedural_score,
        "dominant": format!("{:?}", profile.dominant_paradigm()),
        "guidance": "Weights are heuristic regex-derived scores from \
                     libgrammstein's ParadigmDetector. `dominant` is the highest \
                     score's paradigm name. Use as a sanity-check or trend signal, \
                     not a normative classification."
    }))
}
