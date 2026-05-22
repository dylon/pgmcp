//! `tool_bus_factor` — Bus factor per file (SOTA Phase 4.1, Avelino et al. ICSE 2016).
//!
//! Greedy author-removal until ≥50% LOC unmaintained. Uses `file_chunks.blame_author`.

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::BusFactorParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_bus_factor(
    ctx: &SystemContext,
    params: BusFactorParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "bus_factor", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;

    // For each file, count chunks per author.
    let rows: Vec<(i64, String, String, i64)> = sqlx::query_as::<_, (i64, String, String, i64)>(
        "SELECT f.id, f.relative_path, COALESCE(fc.blame_author, '<unknown>') AS author, COUNT(*)::int8 AS lines
         FROM indexed_files f
         JOIN file_chunks fc ON fc.file_id = f.id
         WHERE f.project_id = $1
         GROUP BY f.id, f.relative_path, author",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Bus factor query failed: {}", e), None))?;

    let mut per_file: HashMap<(i64, String), Vec<(String, i64)>> = HashMap::new();
    for (fid, path, author, lines) in rows {
        per_file
            .entry((fid, path))
            .or_default()
            .push((author, lines));
    }

    let limit = params.limit.unwrap_or(30);
    let threshold = params.threshold.unwrap_or(0.5);

    let mut out: Vec<(String, i32, Vec<String>)> = Vec::with_capacity(per_file.len());
    for ((_fid, path), mut authors) in per_file {
        authors.sort_by_key(|a| std::cmp::Reverse(a.1));
        let total: i64 = authors.iter().map(|(_, l)| *l).sum();
        if total == 0 {
            continue;
        }
        let stop_at = ((total as f64) * threshold) as i64;
        let mut removed: i64 = 0;
        let mut bus: i32 = 0;
        let mut removed_authors: Vec<String> = Vec::new();
        for (author, lines) in authors {
            removed += lines;
            bus += 1;
            removed_authors.push(author);
            if removed >= stop_at {
                break;
            }
        }
        out.push((path, bus, removed_authors));
    }
    out.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    out.truncate(limit.max(0) as usize);

    let files: Vec<_> = out
        .iter()
        .map(|(p, b, a)| json!({"file": p, "bus_factor": b, "top_authors_at_threshold": a}))
        .collect();
    json_result(&json!({
        "project": params.project,
        "threshold": threshold,
        "files": files,
        "guidance": "Bus factor = min # of authors whose departure would leave >= threshold of the file unmaintained. Bus factor of 1 = single point of failure."
    }))
}
