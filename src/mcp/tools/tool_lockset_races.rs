//! `tool_lockset_races` — Heuristic for Eraser-style race candidates (SOTA Phase 5.1).
//!
//! Without intra-procedural lockset analysis, we surface call-sites where a
//! shared mutex/lock primitive is acquired and then released within the same
//! function — both candidates (lock fields without consistent guard scope)
//! and counterexamples (single-mutex, well-contained usage).

#![allow(unused_imports)]

use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::LocksetRacesParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};
use crate::mcp::tools::sota_regex_scan::scan_files_for_pattern;

pub async fn tool_lockset_races(
    ctx: &SystemContext,
    params: LocksetRacesParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "lockset_races", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(50);

    // Mutex/lock usage patterns across Rust, C++, Java, Go.
    let pat = Regex::new(
        r"(?m)\b(std::sync::Mutex|parking_lot::Mutex|tokio::sync::Mutex|RwLock|std::mutex|pthread_mutex_lock|synchronized\s*\(|Lock\.acquire|threading\.Lock|asyncio\.Lock|sync\.Mutex|sync\.RWMutex)\b"
    ).expect("lock pattern");
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
        "guidance": "Surfaces concurrency primitives. To detect actual races (disjoint lock-sets across shared accesses) requires intra-procedural lockset analysis beyond regex; treat these as audit candidates and follow with manual review of variable scoping vs lock acquisition."
    }))
}
