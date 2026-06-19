//! `tool_compare_files` — MCP tool body, extracted from `super::super::server`.

#![allow(unused_imports)]

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, LoggingLevel};
use serde_json::json;
use tracing::debug;

use crate::context::SystemContext;
use crate::mcp::server::*;

pub async fn tool_compare_files(
    ctx: &SystemContext,
    params: CompareFilesParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    debug!(
        tool = "compare_files",
        file_a = %truncate(&params.file_a, 200),
        file_b = %truncate(&params.file_b, 200),
        "MCP tool invoked",
    );

    let ref_a = ctx
        .db()
        .resolve_file_reference(&params.file_a)
        .await
        .map_err(|e| McpError::internal_error(format!("Resolve file_a failed: {}", e), None))?
        .ok_or_else(|| {
            McpError::internal_error(format!("File not found: {}", params.file_a), None)
        })?;

    let ref_b = ctx
        .db()
        .resolve_file_reference(&params.file_b)
        .await
        .map_err(|e| McpError::internal_error(format!("Resolve file_b failed: {}", e), None))?
        .ok_or_else(|| {
            McpError::internal_error(format!("File not found: {}", params.file_b), None)
        })?;

    let ef_search = ctx.config().load().vector.ef_search;
    let pairs = ctx
        .db()
        .compare_two_files(ref_a.file_id, ref_b.file_id, ef_search)
        .await
        .map_err(|e| McpError::internal_error(format!("Comparison failed: {}", e), None))?;

    // Greedy bipartite matching: match each chunk from A to best available chunk from B
    let mut used_b = std::collections::HashSet::new();
    let mut matched_pairs = Vec::new();
    let mut total_weighted_sim = 0.0f64;
    let mut total_weight = 0.0f64;

    for pair in &pairs {
        if used_b.contains(&pair.chunk_id_b) {
            continue;
        }
        // Check if this A chunk is already matched
        if matched_pairs
            .iter()
            .any(|p: &crate::db::queries::ChunkPairSimilarity| p.chunk_id_a == pair.chunk_id_a)
        {
            continue;
        }
        used_b.insert(pair.chunk_id_b);
        let weight_a = (pair.end_line_a - pair.start_line_a + 1) as f64;
        let weight_b = (pair.end_line_b - pair.start_line_b + 1) as f64;
        let weight = (weight_a + weight_b) / 2.0;
        total_weighted_sim += pair.similarity * weight;
        total_weight += weight;
        matched_pairs.push(pair.clone());
    }

    let overall_similarity = if total_weight > 0.0 {
        total_weighted_sim / total_weight
    } else {
        0.0
    };

    let verdict = if overall_similarity >= 0.95 {
        "near-identical"
    } else if overall_similarity >= 0.85 {
        "highly similar"
    } else if overall_similarity >= 0.70 {
        "moderately similar"
    } else {
        "different"
    };

    // Shadow-ASR channel (Phase D2b): per-file effect distribution. For
    // each file, list the effects carried by symbols in that file.
    // Useful for confirming that two files which look similar by content
    // also carry similar effect surfaces.
    type EffectRow = (String, i64);
    let per_file_effects = |file_id: i64| async move {
        let Some(pool) = ctx.db().pool() else {
            return Vec::<serde_json::Value>::new();
        };
        let rows: Vec<EffectRow> = sqlx::query_as(
            "SELECT se.effect, COUNT(*)::int8
             FROM symbol_effects se
             JOIN file_symbols fs ON fs.id = se.symbol_id
             WHERE fs.file_id = $1
             GROUP BY se.effect
             ORDER BY se.effect",
        )
        .bind(file_id)
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        rows.into_iter()
            .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
            .collect()
    };
    let effects_a = per_file_effects(ref_a.file_id).await;
    let effects_b = per_file_effects(ref_b.file_id).await;

    // Shadow-ASR channel (Phase D2b): project-scoped effect distribution.
    let effect_breakdown = match ctx.db().pool() {
        Some(pool) => {
            let pid =
                crate::mcp::tools::sema_helpers::effects::project_id_for_path(pool, &params.file_a)
                    .await;
            crate::mcp::tools::sema_helpers::effects::effect_breakdown_json(pool, pid).await
        }
        None => serde_json::json!({}),
    };

    let result = serde_json::json!({
        "effect_breakdown": effect_breakdown,
        "file_a": {
            "path": ref_a.path,
            "project": ref_a.project_name,
            "language": ref_a.language,
            "line_count": ref_a.line_count,
            "effects": effects_a,
        },
        "file_b": {
            "path": ref_b.path,
            "project": ref_b.project_name,
            "language": ref_b.language,
            "line_count": ref_b.line_count,
            "effects": effects_b,
        },
        "overall_similarity": format!("{:.4}", overall_similarity),
        "verdict": verdict,
        "matched_chunks": matched_pairs.len(),
        "chunk_alignment": matched_pairs.iter().map(|p| serde_json::json!({
            "lines_a": format!("{}-{}", p.start_line_a, p.end_line_a),
            "lines_b": format!("{}-{}", p.start_line_b, p.end_line_b),
            "similarity": format!("{:.4}", p.similarity),
            "snippet_a": truncate(&p.content_a, 200),
            "snippet_b": truncate(&p.content_b, 200),
        })).collect::<Vec<_>>(),
    });

    let json = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "compare_files",
        overall_similarity = %format!("{:.4}", overall_similarity),
        verdict,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
