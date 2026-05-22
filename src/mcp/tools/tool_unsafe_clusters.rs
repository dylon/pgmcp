//! `tool_unsafe_clusters` — Rust unsafe-block density (SOTA Phase 5.2, Astrauskas OOPSLA 2020).

#![allow(unused_imports)]

use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::UnsafeClustersParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};
use crate::mcp::tools::sota_regex_scan::scan_files_for_pattern;

pub async fn tool_unsafe_clusters(
    ctx: &SystemContext,
    params: UnsafeClustersParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "unsafe_clusters", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;

    let pat = Regex::new(r"(?m)\bunsafe\s*(\{|fn|impl|trait)\b").expect("unsafe pattern");
    let hits = scan_files_for_pattern(pool, project_id, &pat, Some(&["rust"]), 100_000)
        .await
        .map_err(|e| McpError::internal_error(format!("Scan failed: {}", e), None))?;

    let mut counts: HashMap<String, (u32, String)> = HashMap::new();
    for h in &hits {
        let entry = counts
            .entry(h.relative_path.clone())
            .or_insert((0, h.language.clone()));
        entry.0 += 1;
    }
    let limit = params.limit.unwrap_or(25);
    let mut rows: Vec<(String, u32)> = counts.into_iter().map(|(p, (c, _))| (p, c)).collect();
    rows.sort_by_key(|a| std::cmp::Reverse(a.1));
    rows.truncate(limit.max(0) as usize);
    let files: Vec<_> = rows
        .iter()
        .map(|(p, c)| json!({"file": p, "unsafe_blocks": c}))
        .collect();
    json_result(&json!({
        "project": params.project,
        "files": files,
        "total_unsafe_blocks": hits.len(),
        "guidance": "Files with dense unsafe blocks merit review priority. Astrauskas et al. OOPSLA 2020 found that unsafe density is concentrated in a small fraction of crates — outliers are review targets."
    }))
}
