//! `tool_compare_files` — MCP tool body, extracted from `super::super::server`.

#![allow(unused_imports)]

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, LoggingLevel};
use serde_json::json;
use tracing::{debug, error, info, warn};

use crate::context::SystemContext;
use crate::mcp::server::*;

pub async fn tool_compare_files(
    ctx: &SystemContext,
    params: CompareFilesParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    info!(
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

    let result = serde_json::json!({
        "file_a": {
            "path": ref_a.path,
            "project": ref_a.project_name,
            "language": ref_a.language,
            "line_count": ref_a.line_count,
        },
        "file_b": {
            "path": ref_b.path,
            "project": ref_b.project_name,
            "language": ref_b.language,
            "line_count": ref_b.line_count,
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
