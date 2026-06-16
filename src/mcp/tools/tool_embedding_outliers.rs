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

    // For each chunk, fetch its k nearest neighbours (id + cosine distance) via
    // the HNSW index, then compute TRUE LRD-LOF (Breunig 2000) in Rust: an
    // outlier is a chunk whose local reachability density is much lower than its
    // neighbours' — caught even when its absolute distance is unremarkable.
    let sql = format!(
        "WITH project_chunks AS (
            SELECT fc.id, f.relative_path, fc.start_line, fc.end_line, fc.{col} AS emb
            FROM file_chunks fc
            JOIN indexed_files f ON fc.file_id = f.id
            WHERE f.project_id = $1 AND fc.{col} IS NOT NULL
        )
        SELECT a.id AS chunk_id, a.relative_path, a.start_line, a.end_line,
               b.id AS neighbor_id, (a.emb <=> b.emb)::float8 AS dist
        FROM project_chunks a
        JOIN LATERAL (
            SELECT b.id, b.emb
            FROM project_chunks b
            WHERE b.id <> a.id
            ORDER BY a.emb <=> b.emb
            LIMIT $2
        ) b ON true
        ORDER BY a.id, dist"
    );
    let rows: Vec<(i64, String, i32, i32, i64, f64)> =
        sqlx::query_as::<_, (i64, String, i32, i32, i64, f64)>(sqlx::AssertSqlSafe(sql.as_str()))
            .bind(project_id)
            .bind(k)
            .fetch_all(pool)
            .await
            .map_err(|e| McpError::internal_error(format!("LOF query failed: {}", e), None))?;

    // Assign each chunk a dense index; build the k-NN adjacency the LOF kernel
    // expects (neighbor indices must reference back into the point set).
    let mut id_to_idx: HashMap<i64, usize> = HashMap::new();
    let mut meta: Vec<(i64, String, i32, i32)> = Vec::new();
    for (cid, path, sl, el, _nid, _d) in &rows {
        if !id_to_idx.contains_key(cid) {
            id_to_idx.insert(*cid, meta.len());
            meta.push((*cid, path.clone(), *sl, *el));
        }
    }
    let n = meta.len();
    let mut neighbors: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
    let mut mean_knn: Vec<f64> = vec![0.0; n];
    let mut mean_cnt: Vec<u32> = vec![0; n];
    for (cid, _p, _sl, _el, nid, d) in &rows {
        let (Some(&ci), Some(&ni)) = (id_to_idx.get(cid), id_to_idx.get(nid)) else {
            continue;
        };
        neighbors[ci].push((ni, *d));
        mean_knn[ci] += *d;
        mean_cnt[ci] += 1;
    }

    let lof = crate::code_analysis::lof::local_outlier_factors(&neighbors, k as usize);

    let mut findings: Vec<serde_json::Value> = Vec::new();
    for i in 0..n {
        if lof[i] >= threshold {
            let mean_d = if mean_cnt[i] > 0 {
                mean_knn[i] / mean_cnt[i] as f64
            } else {
                0.0
            };
            findings.push(json!({
                "chunk_id": meta[i].0,
                "file": meta[i].1.clone(),
                "start_line": meta[i].2,
                "end_line": meta[i].3,
                "lof": lof[i],
                "mean_knn_distance": mean_d,
            }));
        }
    }
    findings.sort_by(|a, b| {
        let av = a["lof"].as_f64().unwrap_or(0.0);
        let bv = b["lof"].as_f64().unwrap_or(0.0);
        bv.partial_cmp(&av).unwrap_or(std::cmp::Ordering::Equal)
    });
    findings.truncate(limit.max(0) as usize);
    let global_mean: f64 = if n > 0 {
        mean_knn.iter().sum::<f64>() / mean_knn.len() as f64
    } else {
        0.0
    };
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
        "guidance": "True LRD-LOF (Breunig et al. SIGMOD 2000): `lof` is the ratio of a chunk's neighbours' \
            local reachability density to its own. LOF ≈ 1 is an inlier; LOF ≫ threshold (default 1.5) means \
            the chunk is far sparser than its neighbourhood — a genuine local outlier, caught even when its \
            absolute distance is ordinary (unlike the prior mean-distance heuristic). Distinct from topic \
            orphans — these are chunks unlike *any* neighbourhood."
    }))
}
