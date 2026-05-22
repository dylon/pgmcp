//! `tool_deprecated_but_used` — Symbols annotated as deprecated but still called
//! (SOTA Phase 7.3).

#![allow(unused_imports)]

use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::DeprecatedButUsedParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_deprecated_but_used(
    ctx: &SystemContext,
    params: DeprecatedButUsedParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "deprecated_but_used", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(30);

    // Find deprecated functions by scanning file contents for the deprecation
    // markers immediately preceding a fn/def/class definition.
    let rows: Vec<(String, Option<String>)> = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT relative_path, content FROM indexed_files WHERE project_id = $1 AND content IS NOT NULL",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Scan failed: {}", e), None))?;

    let deprecated_re = Regex::new(
        r"(?m)#\[deprecated[^\]]*\]\s*(?:pub\s+)?(?:async\s+)?(?:fn|struct|enum|trait|const|static|type)\s+([A-Za-z_][A-Za-z0-9_]*)|@Deprecated[\s\S]{0,200}?\b(?:public|protected)?\s*(?:static\s+)?[A-Za-z_<>][A-Za-z0-9_<>]*\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(|warnings\.warn\([^)]*DeprecationWarning|@deprecated[\s\S]{0,200}?\b(function|class)\s+([A-Za-z_][A-Za-z0-9_]*)"
    ).expect("deprecated regex");
    let mut deprecated_symbols: HashSet<String> = HashSet::new();
    for (_, content) in &rows {
        let Some(c) = content else { continue };
        for cap in deprecated_re.captures_iter(c) {
            if let Some(n) = cap.get(1) {
                deprecated_symbols.insert(n.as_str().to_string());
            } else if let Some(n) = cap.get(2) {
                deprecated_symbols.insert(n.as_str().to_string());
            } else if let Some(n) = cap.get(4) {
                deprecated_symbols.insert(n.as_str().to_string());
            }
        }
    }
    if deprecated_symbols.is_empty() {
        return json_result(&json!({
            "project": params.project,
            "deprecated_symbols": [],
            "guidance": "No deprecation markers found in project sources.",
        }));
    }
    let dep_vec: Vec<String> = deprecated_symbols.iter().cloned().collect();

    // Count incoming call references per deprecated symbol via symbol_references.
    let counts: Vec<(String, i64)> = sqlx::query_as::<_, (String, i64)>(
        "SELECT sr.target_raw, COUNT(*)::int8
         FROM symbol_references sr
         JOIN indexed_files f ON sr.source_file_id = f.id
         WHERE f.project_id = $1 AND sr.target_raw = ANY($2::text[])
         GROUP BY sr.target_raw",
    )
    .bind(project_id)
    .bind(&dep_vec)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Reference query failed: {}", e), None))?;
    let mut rows_out: Vec<(String, i64)> = counts;
    rows_out.sort_by_key(|a| std::cmp::Reverse(a.1));
    rows_out.truncate(limit.max(0) as usize);
    let used: Vec<_> = rows_out
        .iter()
        .map(|(n, c)| json!({"symbol": n, "call_sites": c}))
        .collect();
    json_result(&json!({
        "project": params.project,
        "deprecated_count": deprecated_symbols.len(),
        "still_used": used,
        "guidance": "Deprecated symbols still called from anywhere block their removal. Migrate callers, then delete."
    }))
}
