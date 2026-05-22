//! `tool_clone_density` — Per-file count of `.clone()` / `.to_string()` /
//! `Arc::clone` weighted by PageRank centrality (SOTA Phase 5.9).

#![allow(unused_imports)]

use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::CloneDensityParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};
use crate::mcp::tools::sota_regex_scan::scan_files_for_pattern;

pub async fn tool_clone_density(
    ctx: &SystemContext,
    params: CloneDensityParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "clone_density", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(30);

    let pat =
        Regex::new(r"\.clone\(\)|\.to_string\(\)|Arc::clone\(|Rc::clone\(").expect("clone regex");
    let hits = scan_files_for_pattern(pool, project_id, &pat, Some(&["rust"]), 100_000)
        .await
        .map_err(|e| McpError::internal_error(format!("Scan failed: {}", e), None))?;

    let mut counts: HashMap<String, u32> = HashMap::new();
    for h in hits {
        *counts.entry(h.relative_path).or_insert(0) += 1;
    }
    // Join with file_metrics.pagerank for centrality weighting.
    let pr_rows: Vec<(String, Option<f64>)> = sqlx::query_as::<_, (String, Option<f64>)>(
        "SELECT f.relative_path, fm.pagerank
         FROM file_metrics fm
         JOIN indexed_files f ON f.id = fm.file_id
         WHERE fm.project_id = $1",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("PR lookup failed: {}", e), None))?;
    let pr: HashMap<String, f64> = pr_rows
        .into_iter()
        .filter_map(|(p, v)| v.map(|x| (p, x)))
        .collect();

    let mut rows: Vec<(String, u32, f64, f64)> = counts
        .into_iter()
        .map(|(p, c)| {
            let r = pr.get(&p).copied().unwrap_or(0.0);
            let weighted = c as f64 * (1.0 + r);
            (p, c, r, weighted)
        })
        .collect();
    rows.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
    rows.truncate(limit.max(0) as usize);
    let files: Vec<_> = rows
        .iter()
        .map(|(p, c, r, w)| json!({"file": p, "clones": c, "pagerank": r, "weighted_score": w}))
        .collect();
    json_result(&json!({
        "project": params.project,
        "files": files,
        "guidance": "Clone density × PageRank surfaces allocation hotspots before profiling. High-centrality + many `.clone()` calls = likely refactor target."
    }))
}
