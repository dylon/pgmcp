//! `tool_quadratic_loops` — Detect accidentally-quadratic loops (SOTA Phase 5.6, Petrashko ICSE 2017).
//!
//! Heuristic: a for/while loop containing a `.contains` / `.find` /
//! `.indexOf` / `.includes` call on a collection bound outside the loop.

#![allow(unused_imports)]

use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::QuadraticLoopsParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};
use crate::mcp::tools::sota_regex_scan::scan_files_for_pattern;

pub async fn tool_quadratic_loops(
    ctx: &SystemContext,
    params: QuadraticLoopsParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "quadratic_loops", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(50);

    // Crude two-pass: find lines inside a for/while loop that perform a
    // membership check via an O(n) method. The regex captures the inner
    // call directly; the loop context is implicit (a real CFG-walker would
    // do better, but this surfaces hot candidates).
    let pat = Regex::new(
        r"(?m)^\s*(for|while)\s+[^{]*\{[^}]*\.(contains|find|index_of|indexOf|includes|position)\(",
    )
    .expect("quad regex");
    let hits = scan_files_for_pattern(pool, project_id, &pat, None, limit.max(0) as usize)
        .await
        .map_err(|e| McpError::internal_error(format!("Scan failed: {}", e), None))?;
    let rows: Vec<_> = hits
        .into_iter()
        .map(|h| json!({"file": h.relative_path, "language": h.language, "line": h.line, "snippet": h.snippet}))
        .collect();
    json_result(&json!({
        "project": params.project,
        "matches": rows,
        "guidance": "Accidentally-quadratic = outer loop running an O(n) membership test on each iteration. Replace inner collection with a HashSet for O(1) lookup."
    }))
}
