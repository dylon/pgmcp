//! Memory-server Phase 3.2: pgmcp retrieval extensions.
//!
//! Eight tools layered on top of the official-compat CRUD from
//! `tool_memory_crud`:
//!
//! - `memory_semantic_search` — BGE-M3 vector search over observations
//! - `memory_hybrid_search`   — RRF fusion of FTS + dense
//! - `memory_facts_at`        — bi-temporal point-in-time snapshot
//! - `memory_relations_traverse` — depth-bounded BFS over relations
//! - `memory_anchor_entity` / `memory_unanchor_entity` /
//!   `memory_find_code_for_entity` / `memory_find_entities_for_code` —
//!   code-graph ↔ memory-graph cross-linking

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use tracing::error;
use uuid::Uuid;

use crate::context::SystemContext;
use crate::db::queries;
use crate::mcp::server::{
    MemoryAnchorEntityParams, MemoryFactsAtParams, MemoryFindCodeForEntityParams,
    MemoryFindEntitiesForCodeParams, MemoryHybridSearchParams, MemoryRelationsTraverseParams,
    MemoryScopeParam, MemorySemanticSearchParams, MemoryUnanchorEntityParams,
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

fn parse_scope(p: Option<&MemoryScopeParam>) -> Result<queries::ScopeSpec, McpError> {
    let Some(p) = p else {
        return Ok(queries::ScopeSpec::default());
    };
    let session_id = match p.session_id.as_deref() {
        Some(s) => Some(Uuid::parse_str(s).map_err(|e| {
            McpError::invalid_params(format!("invalid session_id UUID: {}", e), None)
        })?),
        None => None,
    };
    Ok(queries::ScopeSpec {
        user_id: p.user_id.clone(),
        agent_id: p.agent_id.clone(),
        session_id,
        project_id: p.project_id,
    })
}

async fn resolve_optional_scope_id(
    ctx: &SystemContext,
    pool: &sqlx::PgPool,
    scope: Option<&MemoryScopeParam>,
) -> Result<Option<i64>, McpError> {
    let _ = ctx;
    let Some(scope) = scope else {
        return Ok(None);
    };
    let spec = parse_scope(Some(scope))?;
    let id = queries::find_or_create_scope(pool, &spec)
        .await
        .map_err(|e| McpError::internal_error(format!("scope: {}", e), None))?;
    Ok(Some(id))
}

const VALID_TIERS: &[&str] = &[
    "working",
    "episodic",
    "semantic",
    "procedural",
    "reflective",
];

fn validate_tier(t: Option<&str>) -> Result<(), McpError> {
    if let Some(tier) = t
        && !VALID_TIERS.contains(&tier)
    {
        return Err(McpError::invalid_params(
            format!("invalid tier '{}'; must be one of {:?}", tier, VALID_TIERS),
            None,
        ));
    }
    Ok(())
}

pub async fn tool_memory_semantic_search(
    ctx: &SystemContext,
    params: MemorySemanticSearchParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "memory_semantic_search", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    let scope_id = resolve_optional_scope_id(ctx, pool, params.scope.as_ref()).await?;
    let query = params.query.trim();
    if query.is_empty() {
        return Err(McpError::invalid_params("query must be non-empty", None));
    }
    let tier = params
        .tier
        .as_deref()
        .map(str::trim)
        .filter(|tier| !tier.is_empty());
    validate_tier(tier)?;
    let limit = params.limit.unwrap_or(20).clamp(1, 200);

    let embedding = ctx.embed().embed_query(query).await.map_err(|e| {
        error!(tool = "memory_semantic_search", error = %e, "embedding failed");
        McpError::internal_error(format!("embedding failed: {}", e), None)
    })?;

    let ef = ctx.config().load().vector.ef_search;
    let rows = queries::memory_semantic_search(pool, &embedding, scope_id, tier, limit, ef)
        .await
        .map_err(|e| McpError::internal_error(format!("query failed: {}", e), None))?;
    json_result(json!({
        "count": rows.len(),
        "limit": limit,
        "mode": "semantic",
        "results": rows,
    }))
}

pub async fn tool_memory_hybrid_search(
    ctx: &SystemContext,
    params: MemoryHybridSearchParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "memory_hybrid_search", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    let scope_id = resolve_optional_scope_id(ctx, pool, params.scope.as_ref()).await?;
    validate_tier(params.tier.as_deref())?;
    let limit = params.limit.unwrap_or(20);

    let embedding = ctx
        .embed()
        .embed_query(&params.query)
        .await
        .map_err(|e| McpError::internal_error(format!("embedding failed: {}", e), None))?;
    let ef = ctx.config().load().vector.ef_search;

    let rows = queries::memory_hybrid_search(
        pool,
        &params.query,
        &embedding,
        scope_id,
        params.tier.as_deref(),
        limit,
        ef,
    )
    .await
    .map_err(|e| McpError::internal_error(format!("query failed: {}", e), None))?;
    json_result(json!({
        "count": rows.len(),
        "mode": "hybrid_rrf",
        "results": rows,
    }))
}

pub async fn tool_memory_facts_at(
    ctx: &SystemContext,
    params: MemoryFactsAtParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "memory_facts_at", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    let scope_id = resolve_optional_scope_id(ctx, pool, params.scope.as_ref()).await?;
    validate_tier(params.tier.as_deref())?;
    let limit = params.limit_entities.unwrap_or(200);
    let as_of = params
        .as_of
        .as_deref()
        .map(|s| {
            chrono::DateTime::parse_from_rfc3339(s)
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .map_err(|e| {
                    McpError::invalid_params(
                        format!("invalid as_of RFC3339 timestamp: {}", e),
                        None,
                    )
                })
        })
        .transpose()?
        .unwrap_or_else(chrono::Utc::now);
    let snapshot = queries::memory_facts_at(pool, as_of, scope_id, params.tier.as_deref(), limit)
        .await
        .map_err(|e| McpError::internal_error(format!("query failed: {}", e), None))?;
    json_result(json!(snapshot))
}

pub async fn tool_memory_relations_traverse(
    ctx: &SystemContext,
    params: MemoryRelationsTraverseParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "memory_relations_traverse", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    if params.seed_entity_ids.is_empty() {
        return Err(McpError::invalid_params(
            "seed_entity_ids must not be empty",
            None,
        ));
    }
    let max_depth = params.max_depth.unwrap_or(2);
    let max_nodes = params.max_nodes.unwrap_or(200);
    let result = queries::memory_relations_traverse(
        pool,
        &params.seed_entity_ids,
        max_depth,
        params.relation_filter.as_deref(),
        max_nodes,
    )
    .await
    .map_err(|e| McpError::internal_error(format!("query failed: {}", e), None))?;
    json_result(json!(result))
}

pub async fn tool_memory_anchor_entity(
    ctx: &SystemContext,
    params: MemoryAnchorEntityParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "memory_anchor_entity", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    if params.anchor_type.trim().is_empty() {
        return Err(McpError::invalid_params(
            "anchor_type must not be empty",
            None,
        ));
    }
    let id = queries::memory_anchor_entity(
        pool,
        params.entity_id,
        params.file_id,
        params.chunk_id,
        params.topic_id,
        params.symbol_id,
        params.project_id,
        &params.anchor_type,
    )
    .await
    .map_err(|e| match &e {
        sqlx::Error::Protocol(msg) => McpError::invalid_params(msg.clone(), None),
        _ => McpError::internal_error(format!("query failed: {}", e), None),
    })?;
    json_result(json!({
        "anchor_id": id,
        "entity_id": params.entity_id,
        "anchor_type": params.anchor_type,
    }))
}

pub async fn tool_memory_unanchor_entity(
    ctx: &SystemContext,
    params: MemoryUnanchorEntityParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "memory_unanchor_entity", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    let removed = queries::memory_unanchor_entity(pool, params.anchor_id)
        .await
        .map_err(|e| McpError::internal_error(format!("query failed: {}", e), None))?;
    json_result(json!({
        "anchor_id": params.anchor_id,
        "removed": removed,
    }))
}

pub async fn tool_memory_find_code_for_entity(
    ctx: &SystemContext,
    params: MemoryFindCodeForEntityParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "memory_find_code_for_entity", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    let rows =
        queries::memory_find_code_for_entity(pool, params.entity_id, params.anchor_type.as_deref())
            .await
            .map_err(|e| McpError::internal_error(format!("query failed: {}", e), None))?;
    json_result(json!({
        "count": rows.len(),
        "anchors": rows,
    }))
}

pub async fn tool_memory_find_entities_for_code(
    ctx: &SystemContext,
    params: MemoryFindEntitiesForCodeParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "memory_find_entities_for_code", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    let rows = queries::memory_find_entities_for_code(
        pool,
        params.file_id,
        params.chunk_id,
        params.topic_id,
        params.symbol_id,
        params.project_id,
    )
    .await
    .map_err(|e| match &e {
        sqlx::Error::Protocol(msg) => McpError::invalid_params(msg.clone(), None),
        _ => McpError::internal_error(format!("query failed: {}", e), None),
    })?;
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
        "anchors": rows,
    }))
}
