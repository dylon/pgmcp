//! `tool_io_hotpath` — I/O calls weighted by PageRank centrality (SOTA Phase 5.10).

#![allow(unused_imports)]

use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::IoHotpathParams;
use crate::mcp::tools::sema_helpers::effects::symbols_with_any_effect;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};
use crate::mcp::tools::sota_regex_scan::scan_files_for_pattern;
use crate::parsing::type_tags::vocabulary::{EFFECT_FILESYSTEM, EFFECT_IO, EFFECT_NETWORK};

const DEFAULT_IO_HOTPATH_LIMIT: i32 = 30;
const MAX_IO_HOTPATH_LIMIT: i32 = 1_000;
const IO_HOTPATH_SCAN_HIT_CAP: usize = 100_000;
const MAX_IO_HOTPATH_EFFECT_SYMBOLS: usize = 1_000;

pub async fn tool_io_hotpath(
    ctx: &SystemContext,
    params: IoHotpathParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "io_hotpath", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project = params.project.trim().to_string();
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let limit = params
        .limit
        .unwrap_or(DEFAULT_IO_HOTPATH_LIMIT)
        .clamp(1, MAX_IO_HOTPATH_LIMIT) as usize;

    let pat = Regex::new(
        r"\bstd::fs::|tokio::fs::|reqwest::|sqlx::query|rusqlite::|requests\.get|requests\.post|fetch\(|axios\.|open\(.*['\x22]r|open\(.*['\x22]w"
    ).expect("io regex");
    let hits = scan_files_for_pattern(pool, project_id, &pat, None, IO_HOTPATH_SCAN_HIT_CAP)
        .await
        .map_err(|e| McpError::internal_error(format!("Scan failed: {}", e), None))?;
    let scan_truncated = hits.len() >= IO_HOTPATH_SCAN_HIT_CAP;

    let mut counts: HashMap<String, u32> = HashMap::new();
    for h in hits {
        *counts.entry(h.relative_path).or_insert(0) += 1;
    }
    let pr_rows: Vec<(String, Option<f64>, Option<f64>)> =
        sqlx::query_as::<_, (String, Option<f64>, Option<f64>)>(
            "SELECT f.relative_path, fm.pagerank, fm.betweenness
             FROM file_metrics fm
             JOIN indexed_files f
               ON f.id = fm.file_id
              AND f.project_id = fm.project_id
             WHERE fm.project_id = $1
               AND f.project_id = $1",
        )
        .bind(project_id)
        .fetch_all(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("PR lookup failed: {}", e), None))?;
    let pr: HashMap<String, (f64, f64)> = pr_rows
        .into_iter()
        .map(|(p, r, b)| (p, (r.unwrap_or(0.0), b.unwrap_or(0.0))))
        .collect();

    let mut rows: Vec<(String, u32, f64, f64, f64)> = counts
        .into_iter()
        .map(|(p, c)| {
            let (r, b) = pr.get(&p).copied().unwrap_or((0.0, 0.0));
            let weighted = c as f64 * (1.0 + r + b);
            (p, c, r, b, weighted)
        })
        .collect();
    rows.sort_by(|a, b| b.4.partial_cmp(&a.4).unwrap_or(std::cmp::Ordering::Equal));
    let files_truncated = rows.len() > limit;
    rows.truncate(limit);
    let files: Vec<_> = rows
        .iter()
        .map(|(p, c, r, b, w)| {
            json!({
                "file": p,
                "io_calls": c,
                "pagerank": r,
                "betweenness": b,
                "weighted_score": w
            })
        })
        .collect();
    // Shadow-ASR channel: symbols carrying I/O-family effects (io / network / filesystem).
    let io_effect_symbols = symbols_with_any_effect(
        pool,
        project_id,
        &[
            EFFECT_IO.to_string(),
            EFFECT_NETWORK.to_string(),
            EFFECT_FILESYSTEM.to_string(),
        ],
    )
    .await
    .unwrap_or_default()
    .into_iter()
    .take(MAX_IO_HOTPATH_EFFECT_SYMBOLS)
    .map(|(symbol_id, file_id, name, scope_path)| {
        serde_json::json!({
            "symbol_id": symbol_id, "file_id": file_id, "name": name, "scope_path": scope_path,
        })
    })
    .collect::<Vec<_>>();
    json_result(&json!({
        "project": project,
        "limit": limit,
        "scan_hit_cap": IO_HOTPATH_SCAN_HIT_CAP,
        "scan_truncated": scan_truncated,
        "files_truncated": files_truncated,
        "effect_symbol_cap": MAX_IO_HOTPATH_EFFECT_SYMBOLS,
        "files": files,
        "io_effect_symbols": io_effect_symbols,
        "guidance": "I/O × centrality finds hot paths that block on disk/network in critical routing spots. Cache, pool, or move off the synchronous path."
    }))
}
