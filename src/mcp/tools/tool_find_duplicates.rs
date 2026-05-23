//! `tool_find_duplicates` — MCP tool body, extracted from `super::super::server`.

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
use crate::mcp::tools::sema_helpers::equivalence::materialized_available;

pub async fn tool_find_duplicates(
    ctx: &SystemContext,
    params: FindDuplicatesParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let min_sim = params.min_similarity.unwrap_or(0.90);
    let min_projects = params.min_projects.unwrap_or(2);
    let limit = params.limit.unwrap_or(20);
    debug!(
        tool = "find_duplicates",
        min_similarity = min_sim,
        min_projects,
        language = params.language.as_deref().unwrap_or("*"),
        limit,
        "MCP tool invoked",
    );

    let pairs = ctx
        .db()
        .find_duplicate_file_pairs(
            min_sim,
            params.language.as_deref(),
            limit * 5,
            params.include_same_repo.unwrap_or(false),
        )
        .await
        .map_err(|e| McpError::internal_error(format!("Duplicate query failed: {}", e), None))?;

    let clusters = cluster_file_pairs(&pairs, min_projects);
    let limited: Vec<_> = clusters.into_iter().take(limit as usize).collect();

    // Shadow-ASR cross-language channel: pull pairs from the
    // `cross_language_signature_clones` materialized table when the
    // cron has populated it. These are symbol-level (not file-level)
    // matches that the embedding-only clustering above does not surface.
    let mut cross_language_pairs: Vec<serde_json::Value> = Vec::new();
    if let Some(pool) = ctx.db().pool()
        && materialized_available(pool).await.unwrap_or(false)
    {
        type ClonePair = (i64, i64, String, String, f32);
        let rows: Vec<ClonePair> = sqlx::query_as::<_, ClonePair>(
            "SELECT c.symbol_id_a, c.symbol_id_b, c.language_a, c.language_b, c.similarity
             FROM cross_language_signature_clones c
             ORDER BY c.similarity DESC
             LIMIT $1",
        )
        .bind(limit as i64 * 5)
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        for (a, b, lang_a, lang_b, sim) in rows {
            cross_language_pairs.push(json!({
                "symbol_id_a": a,
                "symbol_id_b": b,
                "language_a": lang_a,
                "language_b": lang_b,
                "similarity": sim,
            }));
        }
    }

    // Combined payload: legacy embedding-derived clusters + new
    // shadow-ASR cross-language symbol-pair channel.
    let payload = json!({
        "embedding_clusters": limited,
        "cross_language_symbol_pairs": cross_language_pairs,
    });
    let json = serde_json::to_string_pretty(&payload)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "find_duplicates",
        clusters = limited.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
