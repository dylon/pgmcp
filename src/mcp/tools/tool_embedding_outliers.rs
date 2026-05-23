//! `tool_embedding_outliers` — Local Outlier Factor (LOF) on chunk embeddings
//! (SOTA Phase 8.3, Breunig et al. SIGMOD 2000).

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::EmbeddingOutliersParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_embedding_outliers(
    ctx: &SystemContext,
    params: EmbeddingOutliersParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "embedding_outliers", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let k = params.k.unwrap_or(20).max(1) as i64;
    let threshold = params.threshold.unwrap_or(1.5);
    let limit = params.limit.unwrap_or(30);

    // Phase 5 C7: signature-aware column resolution.
    let active = crate::embed::signature::read_active_signature(pool)
        .await
        .map_err(|e| {
            McpError::internal_error(format!("active embedding signature: {}", e), None)
        })?;
    let col = active.read_column();

    // For each chunk, find its k nearest neighbours and compute a simplified
    // LOF: local_reach_density approximated as 1 / mean k-NN cosine distance.
    // Pure-SQL with pgvector keeps the heavy lifting in Postgres.
    let sql = format!(
        "WITH project_chunks AS (
            SELECT fc.id, f.relative_path, fc.start_line, fc.end_line, fc.{col} AS emb
            FROM file_chunks fc
            JOIN indexed_files f ON fc.file_id = f.id
            WHERE f.project_id = $1 AND fc.{col} IS NOT NULL
        ),
        nn AS (
            SELECT a.id AS chunk_id, a.relative_path, a.start_line, a.end_line,
                   AVG((a.emb <=> b.emb)::float8) AS mean_dist
            FROM project_chunks a
            JOIN LATERAL (
                SELECT b.id, b.emb
                FROM project_chunks b
                WHERE b.id <> a.id
                ORDER BY a.emb <=> b.emb
                LIMIT $2
            ) b ON true
            GROUP BY a.id, a.relative_path, a.start_line, a.end_line
        )
        SELECT chunk_id, relative_path, start_line, end_line, mean_dist FROM nn"
    );
    let rows: Vec<(i64, String, i32, i32, f64)> =
        sqlx::query_as::<_, (i64, String, i32, i32, f64)>(&sql)
            .bind(project_id)
            .bind(k)
            .fetch_all(pool)
            .await
            .map_err(|e| McpError::internal_error(format!("LOF query failed: {}", e), None))?;

    let mut scored: Vec<(i64, String, i32, i32, f64)> = rows;
    let global_mean: f64 = if scored.is_empty() {
        0.0
    } else {
        scored.iter().map(|r| r.4).sum::<f64>() / scored.len() as f64
    };
    // Approximate LOF: chunk's mean k-NN distance / global mean. Outliers >> 1.0.
    let mut findings: Vec<serde_json::Value> = Vec::new();
    if global_mean > 0.0 {
        for r in &mut scored {
            let lof = r.4 / global_mean;
            if lof >= threshold {
                findings.push(json!({
                    "chunk_id": r.0,
                    "file": r.1.clone(),
                    "start_line": r.2,
                    "end_line": r.3,
                    "lof": lof,
                    "mean_knn_distance": r.4,
                }));
            }
        }
    }
    findings.sort_by(|a, b| {
        let av = a["lof"].as_f64().unwrap_or(0.0);
        let bv = b["lof"].as_f64().unwrap_or(0.0);
        bv.partial_cmp(&av).unwrap_or(std::cmp::Ordering::Equal)
    });
    findings.truncate(limit.max(0) as usize);
    // Shadow-ASR channel (Phase D2b): per-effect symbol-count breakdown
    // for the project. Universal enrichment — every tool benefits from
    // surfacing the effect distribution alongside its primary output.
    // Gracefully degrades to empty when the project lookup or
    // shadow-ASR data isn't populated.
    let effect_breakdown: Vec<serde_json::Value> = (async {
        let Some(pool) = ctx.db().pool() else {
            return Vec::new();
        };
        let project_id_opt: Option<i32> =
            sqlx::query_scalar("SELECT id FROM projects WHERE name = $1")
                .bind(&params.project)
                .fetch_optional(pool)
                .await
                .unwrap_or(None);
        match project_id_opt {
            Some(pid) => crate::mcp::tools::sema_helpers::effects::effect_counts(pool, pid)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
                .collect(),
            None => Vec::new(),
        }
    })
    .await;

    json_result(&json!({
        "effect_breakdown": effect_breakdown,
        "project": params.project,
        "k": k,
        "threshold": threshold,
        "global_mean_distance": global_mean,
        "outliers": findings,
        "guidance": "Approximate LOF (Breunig et al. SIGMOD 2000): chunks whose mean k-NN cosine distance is >= threshold × global mean are local outliers. Distinct from topic orphans — these are chunks unlike *any* neighbourhood."
    }))
}
