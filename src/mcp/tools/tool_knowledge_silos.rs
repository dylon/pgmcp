//! `tool_knowledge_silos` — Per-file Gini/Herfindahl on blame (SOTA Phase 4.2).
//!
//! Gini = inequality of ownership; Herfindahl = concentration index. Both
//! peak when a single author owns everything (silo).

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::KnowledgeSilosParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_knowledge_silos(
    ctx: &SystemContext,
    params: KnowledgeSilosParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "knowledge_silos", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;

    let rows: Vec<(String, String, i64)> = sqlx::query_as::<_, (String, String, i64)>(
        "SELECT f.relative_path, COALESCE(fc.blame_author, '<unknown>'), COUNT(*)::int8
         FROM indexed_files f
         JOIN file_chunks fc ON fc.file_id = f.id
         WHERE f.project_id = $1
         GROUP BY f.relative_path, fc.blame_author",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Silo query failed: {}", e), None))?;

    let mut by_file: HashMap<String, Vec<i64>> = HashMap::new();
    for (path, _author, lines) in rows {
        by_file.entry(path).or_default().push(lines);
    }
    let min_herfindahl = params.min_herfindahl.unwrap_or(0.7);
    let limit = params.limit.unwrap_or(30);

    let mut out: Vec<(String, f64, f64, usize)> = Vec::with_capacity(by_file.len());
    for (path, mut shares) in by_file {
        if shares.is_empty() {
            continue;
        }
        let total: i64 = shares.iter().sum();
        if total == 0 {
            continue;
        }
        // Herfindahl-Hirschman index
        let h: f64 = shares
            .iter()
            .map(|&l| {
                let p = l as f64 / total as f64;
                p * p
            })
            .sum();
        // Gini coefficient via sorted shares
        shares.sort();
        let n = shares.len() as f64;
        let mut g_num: f64 = 0.0;
        for (i, s) in shares.iter().enumerate() {
            g_num += (2.0 * (i as f64 + 1.0) - n - 1.0) * (*s as f64);
        }
        let g = g_num / (n * total as f64).max(1.0);
        if h >= min_herfindahl {
            out.push((path, h, g, shares.len()));
        }
    }
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    out.truncate(limit.max(0) as usize);
    let files: Vec<_> = out
        .iter()
        .map(|(p, h, g, n)| {
            json!({
                "file": p,
                "herfindahl": h,
                "gini": g,
                "n_authors": n
            })
        })
        .collect();
    json_result(&json!({
        "project": params.project,
        "min_herfindahl": min_herfindahl,
        "files": files,
        "guidance": "Herfindahl close to 1 = a single author dominates; Gini close to 1 = highly unequal distribution. Both flag knowledge silos."
    }))
}
