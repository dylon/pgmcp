//! `tool_semantic_search` — MCP tool body, extracted from `super::super::server`.

#![allow(unused_imports)]

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, LoggingLevel};
use serde_json::json;
use tracing::{debug, error};

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
    // Shadow-ASR channel (Phase D2b): project-scoped effect distribution.
    let effect_breakdown = match ctx.db().pool() {
        Some(pool) => {
            let pid = crate::mcp::tools::sema_helpers::effects::project_id_opt(
                pool,
                params.project.as_deref(),
            )
            .await;
            crate::mcp::tools::sema_helpers::effects::effect_breakdown_json(pool, pid).await
        }
        None => serde_json::json!({}),
    };

    let mut envelope = serde_json::json!({
        "results": results,
        "effect_breakdown": effect_breakdown,
    });
    crate::mcp::tools::result_shaping::shape_search_results(
        &mut envelope,
        params.snippet_length.map(|n| n.max(0) as usize),
        params.fields.as_deref(),
        crate::mcp::client_profile::current_render_ctx(),
    );

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
