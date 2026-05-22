//! `tool_unsafe_deserialization` — CWE-502 patterns (SOTA Phase 6.4).

#![allow(unused_imports)]

use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::UnsafeDeserializationParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};
use crate::mcp::tools::sota_regex_scan::scan_files_for_pattern;

pub async fn tool_unsafe_deserialization(
    ctx: &SystemContext,
    params: UnsafeDeserializationParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "unsafe_deserialization", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(50);

    let pat = Regex::new(
        r"(?m)\b(pickle\.loads|pickle\.load|cPickle\.loads|yaml\.load\s*\([^)]*\)|ObjectInputStream|unserialize\(|jsonpickle\.decode|marshal\.loads|shelve\.open|Marshal\.load|dill\.loads|deserialize_object)\b"
    ).expect("deserial regex");
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
        "guidance": "Unsafe deserialization (pickle/yaml-load/ObjectInputStream/marshal) executes arbitrary code from attacker-controlled bytes. Replace with safe parsers (json, yaml.safe_load, typed Serde)."
    }))
}
