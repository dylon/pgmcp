//! `tool_phonetic_normalize` (Phase 8, P13.4 real implementation).
//!
//! Applies `PgmcpPhonetics::normalize` (the embedded English
//! Zompist rules by default) to the input term, returns the
//! normalized form plus the phonetic regex-expansion pattern.
//! Self-contained — no external state needed beyond the embedded
//! rule pack.

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
    // P14.4 — resolve via the per-project registry (or fall back to
    // the embedded-English default when the project is unknown /
    // unset).
    let phon = ctx.phonetics_for(params.project.as_deref());
    let normalized = phon.normalize(&params.term);
    let expanded_pattern = phon.expand_to_pattern(&params.term);
    json_result(&json!({
        "input": params.term,
        "normalized": normalized,
        "expanded_pattern": expanded_pattern,
        "language": phon.language().as_str(),
        "project": params.project,
    }))
}
