//! `tool_lsh_clone_detection` — SimHash LSH on chunk embeddings (SOTA Phase 8.1,
//! Indyk-Motwani STOC 1998; Datar et al. SoCG 2004).
//!
//! Random-hyperplane LSH: each chunk's embedding (1024-d BGE-M3) is projected
//! onto 64
//! signed hyperplanes producing a 64-bit signature. Banded LSH (4 bands of
//! 16 bits) finds candidate pairs in O(1). Re-rank by exact cosine.

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::LshCloneDetectionParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

/// Deterministic per-seed Gaussian hyperplanes via xorshift64 + Box-Muller.
fn make_hyperplanes(num_bits: usize, dim: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut state = seed.max(1);
    let next = |state: &mut u64| {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        ((*state) as f64) / (u64::MAX as f64)
    };
    let mut out = Vec::with_capacity(num_bits);
    for _ in 0..num_bits {
        let mut row = Vec::with_capacity(dim);
        for _ in 0..dim {
            // Box-Muller transform
            let u1 = next(&mut state).max(1e-12);
            let u2 = next(&mut state);
            let r = ((-2.0_f64 * u1.ln()).max(0.0)).sqrt();
            row.push((r * (2.0 * std::f64::consts::PI * u2).cos()) as f32);
        }
        out.push(row);
    }
    out
}

fn signature(emb: &[f32], hps: &[Vec<f32>]) -> u64 {
    let mut sig: u64 = 0;
    for (i, h) in hps.iter().enumerate() {
        let mut dot = 0.0_f32;
        for j in 0..emb.len().min(h.len()) {
            dot += emb[j] * h[j];
        }
        if dot >= 0.0 {
            sig |= 1u64 << i;
        }
    }
    sig
}

fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

pub async fn tool_lsh_clone_detection(
    ctx: &SystemContext,
    params: LshCloneDetectionParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "lsh_clone_detection", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let min_similarity = params.min_similarity.unwrap_or(0.85);
    let limit = params.limit.unwrap_or(50);

    // Phase 5 C7: signature-aware column resolution.
    let active = crate::embed::signature::read_active_signature(pool)
        .await
        .map_err(|e| {
            McpError::internal_error(format!("active embedding signature: {}", e), None)
        })?;
    let col = active.read_column();

    let sql = format!(
        "SELECT fc.id, f.relative_path, fc.start_line, fc.end_line, fc.{col}
         FROM file_chunks fc
         JOIN indexed_files f ON fc.file_id = f.id
         WHERE f.project_id = $1 AND fc.{col} IS NOT NULL
         LIMIT 5000"
    );
    let rows: Vec<(i64, String, i32, i32, Option<pgvector::Vector>)> = sqlx::query_as::<
        _,
        (i64, String, i32, i32, Option<pgvector::Vector>),
    >(&sql)
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Embedding query failed: {}", e), None))?;

    if rows.is_empty() {
        return json_result(&json!({
            "project": params.project,
            "pairs": [],
            "guidance": "No embeddings — index the project first.",
        }));
    }
    let dim = rows
        .first()
        .and_then(|r| r.4.as_ref())
        .map(|v| v.as_slice().len())
        .unwrap_or_else(|| active.dim());
    let hyper = make_hyperplanes(64, dim, 42);
    let sigs: Vec<(i64, String, i32, i32, Vec<f32>, u64)> = rows
        .into_iter()
        .filter_map(|(id, path, s, e, emb)| {
            emb.map(|v| {
                let slice: Vec<f32> = v.as_slice().to_vec();
                let sig = signature(&slice, &hyper);
                (id, path, s, e, slice, sig)
            })
        })
        .collect();

    // Bands of 16 bits each.
    let mut buckets: std::collections::HashMap<(u8, u16), Vec<usize>> =
        std::collections::HashMap::new();
    for (idx, item) in sigs.iter().enumerate() {
        for band in 0..4u8 {
            let bits = ((item.5 >> (band * 16)) & 0xFFFF) as u16;
            buckets.entry((band, bits)).or_default().push(idx);
        }
    }

    let mut seen_pairs: std::collections::HashSet<(usize, usize)> =
        std::collections::HashSet::new();
    let mut pairs: Vec<serde_json::Value> = Vec::new();
    for (_, idxs) in buckets {
        if idxs.len() < 2 {
            continue;
        }
        for i in 0..idxs.len() {
            for j in (i + 1)..idxs.len() {
                let (a, b) = (idxs[i].min(idxs[j]), idxs[i].max(idxs[j]));
                if !seen_pairs.insert((a, b)) {
                    continue;
                }
                let sa = &sigs[a];
                let sb = &sigs[b];
                let ham = hamming(sa.5, sb.5);
                // Approx cosine via 1 - hamming/64 (signed-bit Hamming approximation).
                let approx_cos = (std::f64::consts::PI * (ham as f64) / 64.0).cos();
                if approx_cos < min_similarity {
                    continue;
                }
                pairs.push(json!({
                    "chunk_a_id": sa.0,
                    "file_a": sa.1,
                    "lines_a": [sa.2, sa.3],
                    "chunk_b_id": sb.0,
                    "file_b": sb.1,
                    "lines_b": [sb.2, sb.3],
                    "approx_cosine": approx_cos,
                    "hamming": ham,
                }));
                if pairs.len() >= limit.max(0) as usize {
                    break;
                }
            }
            if pairs.len() >= limit.max(0) as usize {
                break;
            }
        }
        if pairs.len() >= limit.max(0) as usize {
            break;
        }
    }
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
        "min_similarity": min_similarity,
        "pairs": pairs,
        "guidance": format!("SimHash on {}-d embeddings ({}) → 64-bit signatures → banded LSH buckets. Use approx_cosine as a screen; verify exact pgvector cosine for top candidates.", active.dim(), active.model_name())
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hyperplanes_deterministic_for_seed() {
        let a = make_hyperplanes(4, 16, 42);
        let b = make_hyperplanes(4, 16, 42);
        assert_eq!(a, b);
    }
    #[test]
    fn signature_changes_with_input() {
        let h = make_hyperplanes(4, 16, 1);
        let v1 = vec![1.0_f32; 16];
        let v2 = vec![-1.0_f32; 16];
        let s1 = signature(&v1, &h);
        let s2 = signature(&v2, &h);
        assert_ne!(s1, s2);
    }
    #[test]
    fn hamming_self_is_zero() {
        assert_eq!(hamming(0xABCD, 0xABCD), 0);
    }
}
