//! `tool_semantic_search` — MCP tool body, extracted from `super::super::server`.

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

pub async fn tool_semantic_search(
    ctx: &SystemContext,
    params: SemanticSearchParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .semantic_searches
        .fetch_add(1, Ordering::Relaxed);

    let limit = params.limit.unwrap_or(10);
    debug!(
        tool = "semantic_search",
        query = %truncate(&params.query, 200),
        limit,
        language = params.language.as_deref().unwrap_or("*"),
        project = params.project.as_deref().unwrap_or("*"),
        "MCP tool invoked",
    );

    // Cold-start fast-fail: surface a clear, retryable signal rather than
    // parking the request in the bounded query channel until a worker finishes
    // loading its model. Only fires during the brief warmup window.
    if !ctx.embed().is_ready() {
        return Err(McpError::internal_error(
            "embedder is still warming up (loading model); retry shortly",
            None,
        ));
    }

    // Embed the query
    let embedding = ctx.embed().embed_query(&params.query).await.map_err(|e| {
        error!(tool = "semantic_search", error = %e, "MCP tool failed");
        McpError::internal_error(format!("Embedding failed: {}", e), None)
    })?;

    let ef_search = ctx.config().load().vector.ef_search;
    let results = ctx
        .db()
        .semantic_search(
            &embedding,
            limit,
            params.language.as_deref(),
            params.project.as_deref(),
            ef_search,
            params.dedupe_worktrees.unwrap_or(false),
        )
        .await
        .map_err(|e| {
            error!(tool = "semantic_search", error = %e, "MCP tool failed");
            McpError::internal_error(format!("Search failed: {}", e), None)
        })?;

    // Shadow-ASR Pattern D filter: post-filter the result set against
    // the enclosing-symbol's return_type_tags / effects / scope_kind.
    let filtered_results = crate::mcp::tools::sema_helpers::filters::enclosing_symbol_filter_pass(
        ctx.db().pool(),
        results,
        params.return_type_tags.as_deref(),
        params.effects.as_deref(),
        params.scope_kind.as_deref(),
    )
    .await;
    let count = filtered_results.len();
    let results = filtered_results;
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
        "results": results,
        "effect_breakdown": effect_breakdown,
    });

    let json = serde_json::to_string_pretty(&envelope)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "semantic_search",
        results = count,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
