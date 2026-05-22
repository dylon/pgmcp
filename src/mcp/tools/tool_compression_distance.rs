//! `tool_compression_distance` — Normalized Compression Distance
//! (SOTA Phase 3.1, Cilibrasi-Vitányi IEEE TIT 2005).

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::graph::info_theory::ncd_pair_symmetric;
use crate::mcp::server::CompressionDistanceParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

pub async fn tool_compression_distance(
    ctx: &SystemContext,
    params: CompressionDistanceParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "compression_distance", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let level = params.level.unwrap_or(3);

    // Fetch both file bodies.
    let rows: Vec<(String, Option<String>)> = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT f.relative_path, f.content
         FROM indexed_files f
         JOIN projects p ON f.project_id = p.id
         WHERE p.name = $1 AND f.relative_path IN ($2, $3)",
    )
    .bind(&params.project)
    .bind(&params.file_a)
    .bind(&params.file_b)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("File fetch failed: {}", e), None))?;

    let a_content = rows
        .iter()
        .find(|(p, _)| p == &params.file_a)
        .and_then(|(_, c)| c.as_deref())
        .ok_or_else(|| {
            McpError::internal_error(
                format!("File {} not found or has no content", params.file_a),
                None,
            )
        })?;
    let b_content = rows
        .iter()
        .find(|(p, _)| p == &params.file_b)
        .and_then(|(_, c)| c.as_deref())
        .ok_or_else(|| {
            McpError::internal_error(
                format!("File {} not found or has no content", params.file_b),
                None,
            )
        })?;

    let d = ncd_pair_symmetric(a_content.as_bytes(), b_content.as_bytes(), level)
        .map_err(|e| McpError::internal_error(format!("NCD computation failed: {}", e), None))?;

    json_result(&json!({
        "project": params.project,
        "file_a": params.file_a,
        "file_b": params.file_b,
        "ncd": d,
        "compressor": "zstd",
        "level": level,
        "interpretation": match d {
            x if x < 0.3 => "highly similar (likely clone or near-clone)",
            x if x < 0.6 => "moderately similar",
            x if x < 0.9 => "weakly similar",
            _ => "unrelated",
        }
    }))
}
