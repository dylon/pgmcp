//! `tool_index_stats` — MCP tool body, extracted from `super::super::server`.

use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use tracing::debug;

use crate::context::SystemContext;

pub async fn tool_index_stats(ctx: &SystemContext) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    debug!(tool = "index_stats", "MCP tool invoked");

    let snapshot = ctx.stats().snapshot();
    let index_counts = match async {
        let projects = ctx.db().count_projects().await?;
        let indexed_files = ctx.db().count_indexed_files().await?;
        let chunks = ctx.db().count_chunks().await?;
        let total_bytes = ctx.db().total_bytes_indexed().await?;
        Ok::<serde_json::Value, sqlx::Error>(serde_json::json!({
            "available": true,
            "projects": projects,
            "indexed_files": indexed_files,
            "chunks": chunks,
            "total_bytes": total_bytes,
        }))
    }
    .await
    {
        Ok(counts) => counts,
        Err(e) => serde_json::json!({
            "available": false,
            "error": e.to_string(),
            "projects": 0,
            "indexed_files": 0,
            "chunks": 0,
            "total_bytes": 0,
        }),
    };

    // Shadow-ASR channel (Phase D2b): workspace-wide effect distribution.
    let effect_breakdown: Vec<serde_json::Value> = (async {
        let Some(pool) = ctx.db().pool() else {
            return Vec::new();
        };
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT se.effect, COUNT(*)::int8
             FROM symbol_effects se
             GROUP BY se.effect
             ORDER BY se.effect",
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        rows.into_iter()
            .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
            .collect()
    })
    .await;

    let envelope = serde_json::json!({
        "snapshot": snapshot,
        "index": index_counts,
        "effect_breakdown": effect_breakdown,
    });

    let json = serde_json::to_string_pretty(&envelope)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "index_stats",
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
