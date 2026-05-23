//! Memory-server Phase 6 graph-enhanced retrieval tools.
//!
//! Five tools layered over the unified-graph view from Phase 6.3 +
//! HippoRAG/PPR (6.2) + RAPTOR (6.1):
//!
//! - `memory_unified_search` — vector retrieval over the heterogeneous
//!   node view.
//! - `memory_neighbors` — BFS over the unified edge view.
//! - `memory_path_search` — PathRAG: ranked + flow-pruned paths.
//! - `memory_ppr_search` — HippoRAG: Personalized PageRank.
//! - `memory_raptor_search` — query the RAPTOR summary tree.
//!
//! Per Phase 6.5, every tool here is opt-in at the call site. The
//! daemon's `[memory.graph_rag]` config governs latency caps and
//! auto-disable thresholds.

use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use tracing::error;

use crate::context::SystemContext;
use crate::db::queries;
use crate::mcp::server::{
    MemoryNeighborsParams, MemoryPathSearchParams, MemoryPprSearchParams, MemoryRaptorSearchParams,
    MemoryUnifiedSearchParams,
};

fn raw_pool(ctx: &SystemContext) -> Result<&sqlx::PgPool, McpError> {
    ctx.db()
        .pool()
        .ok_or_else(|| McpError::internal_error("raw pool unavailable", None))
}

fn json_result(value: serde_json::Value) -> Result<CallToolResult, McpError> {
    let text = serde_json::to_string_pretty(&value)
        .map_err(|e| McpError::internal_error(format!("serialize failed: {}", e), None))?;
    Ok(CallToolResult::success(vec![rmcp::model::Content::text(
        text,
    )]))
}

fn enforce_latency_cap(ctx: &SystemContext, tool: &'static str, elapsed_ms: u64) {
    let cap = ctx.config().load().memory.graph_rag.max_latency_ms;
    if cap > 0 && elapsed_ms as i64 > cap {
        ctx.stats()
            .graph_retrieval_latency_violations
            .fetch_add(1, Ordering::Relaxed);
        tracing::warn!(
            tool,
            elapsed_ms,
            cap_ms = cap,
            "graph_rag: latency cap exceeded"
        );
    }
}

pub async fn tool_memory_unified_search(
    ctx: &SystemContext,
    params: MemoryUnifiedSearchParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "memory_unified_search", "MCP tool invoked");
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    let embedding = ctx.embed().embed_query(&params.query).await.map_err(|e| {
        error!(tool = "memory_unified_search", error = %e, "embed failed");
        McpError::internal_error(format!("embed failed: {}", e), None)
    })?;
    let ef = ctx.config().load().vector.ef_search;
    let limit = params.k.unwrap_or(20);
    let node_types = params.node_types;
    let rows = queries::memory_unified_search(pool, &embedding, node_types.as_deref(), limit, ef)
        .await
        .map_err(|e| McpError::internal_error(format!("query failed: {}", e), None))?;
    enforce_latency_cap(
        ctx,
        "memory_unified_search",
        start.elapsed().as_millis() as u64,
    );
    json_result(json!({
        "count": rows.len(),
        "results": rows,
    }))
}

pub async fn tool_memory_neighbors(
    ctx: &SystemContext,
    params: MemoryNeighborsParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "memory_neighbors", "MCP tool invoked");
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    let depth = params.depth.unwrap_or(1);
    let max_nodes = params.max_nodes.unwrap_or(200);
    let result = queries::memory_neighbors(
        pool,
        &params.node_id,
        depth,
        params.edge_filter.as_deref(),
        max_nodes,
    )
    .await
    .map_err(|e| McpError::internal_error(format!("query failed: {}", e), None))?;
    enforce_latency_cap(ctx, "memory_neighbors", start.elapsed().as_millis() as u64);
    json_result(json!(result))
}

pub async fn tool_memory_path_search(
    ctx: &SystemContext,
    params: MemoryPathSearchParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "memory_path_search", "MCP tool invoked");
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    let embedding = ctx
        .embed()
        .embed_query(&params.query)
        .await
        .map_err(|e| McpError::internal_error(format!("embed failed: {}", e), None))?;
    let ef = ctx.config().load().vector.ef_search;
    let cfg = ctx.config().load();
    let max_hops = params
        .max_hops
        .unwrap_or(cfg.memory.graph_rag.path_search_default_max_hops);
    let prune = params
        .prune_jaccard
        .unwrap_or(cfg.memory.graph_rag.path_search_prune_jaccard) as f64;
    let k = params.k.unwrap_or(10);
    let result = queries::memory_path_search(
        pool,
        &embedding,
        params.seed_node_types.as_deref(),
        params.target_node_types.as_deref(),
        max_hops,
        k,
        prune,
        ef,
    )
    .await
    .map_err(|e| McpError::internal_error(format!("query failed: {}", e), None))?;
    enforce_latency_cap(
        ctx,
        "memory_path_search",
        start.elapsed().as_millis() as u64,
    );
    json_result(json!(result))
}

pub async fn tool_memory_ppr_search(
    ctx: &SystemContext,
    params: MemoryPprSearchParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "memory_ppr_search", "MCP tool invoked");
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    let embedding = ctx
        .embed()
        .embed_query(&params.query)
        .await
        .map_err(|e| McpError::internal_error(format!("embed failed: {}", e), None))?;
    let ef = ctx.config().load().vector.ef_search;
    let alpha = params.alpha.unwrap_or(0.85);
    let max_seeds = params.max_seeds.unwrap_or(10);
    let k = params.k.unwrap_or(20);
    if !(0.0..=1.0).contains(&alpha) {
        return Err(McpError::invalid_params("alpha must be in [0,1]", None));
    }
    let result = queries::memory_ppr_search(pool, &embedding, k, alpha, max_seeds, ef)
        .await
        .map_err(|e| McpError::internal_error(format!("query failed: {}", e), None))?;
    enforce_latency_cap(ctx, "memory_ppr_search", start.elapsed().as_millis() as u64);
    json_result(json!(result))
}

pub async fn tool_memory_raptor_search(
    ctx: &SystemContext,
    params: MemoryRaptorSearchParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "memory_raptor_search", "MCP tool invoked");
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    let embedding = ctx
        .embed()
        .embed_query(&params.query)
        .await
        .map_err(|e| McpError::internal_error(format!("embed failed: {}", e), None))?;
    let ef = ctx.config().load().vector.ef_search;
    let k = params.k.unwrap_or(10);
    let rows = queries::memory_raptor_search(
        pool,
        &embedding,
        params.scope_id,
        params.levels.as_deref(),
        k,
        ef,
    )
    .await
    .map_err(|e| McpError::internal_error(format!("query failed: {}", e), None))?;
    enforce_latency_cap(
        ctx,
        "memory_raptor_search",
        start.elapsed().as_millis() as u64,
    );
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

    json_result(json!({
        "effect_breakdown": effect_breakdown,
        "count": rows.len(),
        "results": rows,
    }))
}
