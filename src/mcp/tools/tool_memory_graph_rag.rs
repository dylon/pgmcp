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

const MAX_MEMORY_UNIFIED_QUERY_BYTES: usize = 16 * 1024;
const MAX_MEMORY_UNIFIED_NODE_TYPES: usize = 32;
const DEFAULT_MEMORY_UNIFIED_K: i32 = 20;
const MAX_MEMORY_UNIFIED_K: i32 = 200;
const MAX_VECTOR_EF_SEARCH: i32 = 10_000;

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

fn valid_node_type_list() -> String {
    crate::db::ontology::NODE_TYPES
        .iter()
        .map(|n| n.key)
        .collect::<Vec<_>>()
        .join(", ")
}

fn normalize_memory_unified_node_types(
    raw: Option<Vec<String>>,
) -> Result<Option<Vec<String>>, McpError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    if raw.is_empty() {
        return Err(McpError::invalid_params(
            "node_types must not be empty when supplied",
            None,
        ));
    }
    if raw.len() > MAX_MEMORY_UNIFIED_NODE_TYPES {
        return Err(McpError::invalid_params(
            format!("node_types must contain at most {MAX_MEMORY_UNIFIED_NODE_TYPES} entries"),
            None,
        ));
    }

    let mut seen = std::collections::BTreeSet::new();
    let mut normalized = Vec::new();
    for node_type in raw {
        let node_type = node_type.trim();
        if node_type.is_empty() {
            return Err(McpError::invalid_params(
                "node_types entries must be non-empty",
                None,
            ));
        }
        if !crate::db::ontology::is_registered_node_type(node_type) {
            return Err(McpError::invalid_params(
                format!(
                    "unknown node_type '{node_type}'; valid types: {}",
                    valid_node_type_list()
                ),
                None,
            ));
        }
        if seen.insert(node_type.to_string()) {
            normalized.push(node_type.to_string());
        }
    }
    Ok(Some(normalized))
}

pub async fn tool_memory_unified_search(
    ctx: &SystemContext,
    params: MemoryUnifiedSearchParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "memory_unified_search", "MCP tool invoked");
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let query = params.query.trim();
    if query.is_empty() {
        return Err(McpError::invalid_params("query must be non-empty", None));
    }
    if query.len() > MAX_MEMORY_UNIFIED_QUERY_BYTES {
        return Err(McpError::invalid_params(
            format!("query must be at most {MAX_MEMORY_UNIFIED_QUERY_BYTES} bytes"),
            None,
        ));
    }
    let node_types = normalize_memory_unified_node_types(params.node_types)?;
    let k = params
        .k
        .unwrap_or(DEFAULT_MEMORY_UNIFIED_K)
        .clamp(1, MAX_MEMORY_UNIFIED_K);
    let ef = ctx
        .config()
        .load()
        .vector
        .ef_search
        .clamp(1, MAX_VECTOR_EF_SEARCH);
    let embedding = ctx.embed().embed_query(query).await.map_err(|e| {
        error!(tool = "memory_unified_search", error = %e, "embed failed");
        McpError::internal_error(format!("embed failed: {}", e), None)
    })?;
    if embedding.len() != 1024 {
        return Err(McpError::internal_error(
            format!(
                "query embedding dimension mismatch: got {}, expected 1024",
                embedding.len()
            ),
            None,
        ));
    }
    let pool = raw_pool(ctx)?;
    let rows = queries::memory_unified_search(pool, &embedding, node_types.as_deref(), k, ef)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Protocol(msg) => McpError::invalid_params(msg.clone(), None),
            _ => McpError::internal_error(format!("query failed: {}", e), None),
        })?;
    enforce_latency_cap(
        ctx,
        "memory_unified_search",
        start.elapsed().as_millis() as u64,
    );
    json_result(json!({
        "count": rows.len(),
        "query": query,
        "node_types": node_types,
        "k": k,
        "ef_search": ef,
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

/// `graph_neighbors` — friendly-ref wrapper over `memory_neighbors`. Resolves a
/// human node reference (`work_item:WI-12`, `file:src/foo.rs`, `project:pgmcp`,
/// `experiment:slug`, `topic:auth`, `symbol:Foo`, `commit:<sha>`, `agent:<id>`,
/// or numeric `chunk:123`) to the composite `node_id`, then traverses the
/// unified knowledge graph. The traversal itself is generic, so this is purely
/// an ergonomic resolver + delegate.
pub async fn tool_graph_neighbors(
    ctx: &SystemContext,
    params: crate::mcp::server::GraphNeighborsParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "graph_neighbors", "MCP tool invoked");
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    let (node_type, key) = params.node_ref.split_once(':').ok_or_else(|| {
        McpError::invalid_params(
            "node_ref must be '<type>:<key>' — e.g. 'work_item:WI-12', 'file:src/foo.rs', \
             'project:pgmcp', 'experiment:my-slug', 'topic:auth', 'symbol:MyType', 'agent:codex', \
             or numeric 'chunk:123'",
            None,
        )
    })?;
    if !crate::db::ontology::is_registered_node_type(node_type) {
        let valid: Vec<&str> = crate::db::ontology::NODE_TYPES
            .iter()
            .map(|n| n.key)
            .collect();
        return Err(McpError::invalid_params(
            format!(
                "unknown node type '{node_type}'; valid types: {}",
                valid.join(", ")
            ),
            None,
        ));
    }
    let node_id = queries::resolve_graph_node_id(pool, node_type, key)
        .await
        .map_err(|e| McpError::internal_error(format!("resolve failed: {}", e), None))?
        .ok_or_else(|| McpError::invalid_params(format!("no {node_type} matches '{key}'"), None))?;
    let depth = params.depth.unwrap_or(1);
    let max_nodes = params.max_nodes.unwrap_or(200);
    let result = queries::memory_neighbors(
        pool,
        &node_id,
        depth,
        params.edge_filter.as_deref(),
        max_nodes,
    )
    .await
    .map_err(|e| McpError::internal_error(format!("query failed: {}", e), None))?;
    enforce_latency_cap(ctx, "graph_neighbors", start.elapsed().as_millis() as u64);
    json_result(json!({
        "node_ref": params.node_ref,
        "resolved_node_id": node_id,
        "neighbors": result,
    }))
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
    // Stage 5b: optional point-in-time (RFC3339) + recency half-life (default 90d).
    let as_of = match params.as_of.as_deref() {
        Some(s) => Some(
            chrono::DateTime::parse_from_rfc3339(s)
                .map_err(|e| McpError::invalid_params(format!("as_of must be RFC3339: {e}"), None))?
                .with_timezone(&chrono::Utc),
        ),
        None => None,
    };
    let half_life_days = params.half_life_days.unwrap_or(90.0).max(0.001);
    let result = queries::memory_path_search(
        pool,
        &embedding,
        params.seed_node_types.as_deref(),
        params.target_node_types.as_deref(),
        max_hops,
        k,
        prune,
        ef,
        as_of,
        half_life_days,
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
    let query = params.query.trim();
    if query.is_empty() {
        return Err(McpError::invalid_params("query must be non-empty", None));
    }
    if let Some(scope_id) = params.scope_id
        && scope_id <= 0
    {
        return Err(McpError::invalid_params(
            "scope_id must be a positive integer",
            None,
        ));
    }
    let levels = queries::normalize_memory_raptor_levels(params.levels.as_deref())
        .map_err(|e| McpError::invalid_params(e, None))?;
    let embedding = ctx
        .embed()
        .embed_query(query)
        .await
        .map_err(|e| McpError::internal_error(format!("embed failed: {}", e), None))?;
    if embedding.len() != 1024 {
        return Err(McpError::internal_error(
            format!(
                "query embedding dimension mismatch: got {}, expected 1024",
                embedding.len()
            ),
            None,
        ));
    }
    let ef = ctx
        .config()
        .load()
        .vector
        .ef_search
        .clamp(1, MAX_VECTOR_EF_SEARCH);
    let k = params.k.unwrap_or(10).clamp(1, 200);
    let rows =
        queries::memory_raptor_search(pool, &embedding, params.scope_id, levels.as_deref(), k, ef)
            .await
            .map_err(|e| McpError::internal_error(format!("query failed: {}", e), None))?;
    enforce_latency_cap(
        ctx,
        "memory_raptor_search",
        start.elapsed().as_millis() as u64,
    );
    // Shadow-ASR channel (Phase D2b): project-scoped effect distribution
    // (resolved from the requested memory scope's owning project).
    let project_id: Option<i32> = match params.scope_id {
        Some(scope_id) => sqlx::query_scalar::<_, i32>(
            "SELECT project_id FROM memory_scope WHERE id = $1 AND project_id IS NOT NULL",
        )
        .bind(scope_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten(),
        None => None,
    };
    let effect_breakdown =
        crate::mcp::tools::sema_helpers::effects::effect_breakdown_json(pool, project_id).await;

    json_result(json!({
        "effect_breakdown": effect_breakdown,
        "count": rows.len(),
        "query": query,
        "scope_id": params.scope_id,
        "levels": levels,
        "k": k,
        "results": rows,
    }))
}
