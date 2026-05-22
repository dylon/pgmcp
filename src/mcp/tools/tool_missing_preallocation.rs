//! `tool_missing_preallocation` — Vec::new()/HashMap::new() followed by a
//! known-bound loop (SOTA Phase 5.7).

#![allow(unused_imports)]

use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::MissingPreallocationParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};
use crate::mcp::tools::sota_regex_scan::scan_files_for_pattern;

pub async fn tool_missing_preallocation(
    ctx: &SystemContext,
    params: MissingPreallocationParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "missing_preallocation", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(50);

    let pat = Regex::new(
        r"(?m)\b(Vec::new\(\)|HashMap::new\(\)|HashSet::new\(\)|BTreeMap::new\(\)|VecDeque::new\(\)|new\s+ArrayList<|new\s+HashMap<|\[\]|\{\}|list\(\)|dict\(\)|set\(\))"
    ).expect("prealloc regex");
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
        "guidance": "Default empty constructors followed by loops can be preallocated when the bound is known (Vec::with_capacity, HashMap::with_capacity). Inspect surrounding code for a known size hint."
    }))
}
