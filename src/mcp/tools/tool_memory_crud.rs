//! Memory-server Phase 3.1: official MCP memory-server compatible CRUD.
//!
//! Implements the 9 tools defined by `@modelcontextprotocol/server-memory`
//! over pgmcp's `memory_*` tables. Drop-in replacement for the official
//! reference implementation — JSON shapes match upstream so agents that
//! target the official server can swap pgmcp in unchanged.
//!
//! All tools accept an optional `scope` object
//! `{user_id?, agent_id?, session_id?, project_id?}`; missing scope
//! resolves to `(NULL, NULL, NULL, NULL)` ("workspace-wide"). Every
//! created entity is attached to the resolved scope via
//! `memory_entity_scope`.
//!
//! Source provenance for these tools is `'agent_write'` — the agent
//! explicitly called CRUD. The LLM-driven extraction path (Phase 4)
//! uses `'llm_extraction'`.

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use tracing::{debug, error};
use uuid::Uuid;

use crate::context::SystemContext;
use crate::db::queries::{
    self, AddObservationInput, DeleteObservationInput, NewEntityInput, NewRelationInput, ScopeSpec,
};
use crate::mcp::server::{
    MemoryAddObservationsParams, MemoryCreateEntitiesParams, MemoryCreateRelationsParams,
    MemoryDeleteEntitiesParams, MemoryDeleteObservationsParams, MemoryDeleteRelationsParams,
    MemoryOpenNodesParams, MemoryReadGraphParams, MemoryScopeParam, MemorySearchNodesParams,
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

const MEMORY_OPEN_NODES_MAX_NAMES: usize = 100;

fn normalize_open_node_names(names: Vec<String>) -> Result<Vec<String>, McpError> {
    if names.is_empty() {
        return Err(McpError::invalid_params("names must not be empty", None));
    }
    if names.len() > MEMORY_OPEN_NODES_MAX_NAMES {
        return Err(McpError::invalid_params(
            format!(
                "names must contain at most {} entries",
                MEMORY_OPEN_NODES_MAX_NAMES
            ),
            None,
        ));
    }

    let mut seen = HashSet::with_capacity(names.len());
    let mut normalized = Vec::with_capacity(names.len());
    for name in names {
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err(McpError::invalid_params(
                "names must not contain blanks",
                None,
            ));
        }
        if seen.insert(name.clone()) {
            normalized.push(name);
        }
    }
    Ok(normalized)
}

fn parse_scope(p: Option<&MemoryScopeParam>) -> Result<ScopeSpec, McpError> {
    let Some(p) = p else {
        return Ok(ScopeSpec::default());
    };
    let session_id = match p.session_id.as_deref() {
        Some(s) => Some(Uuid::parse_str(s).map_err(|e| {
            McpError::invalid_params(format!("invalid session_id UUID: {}", e), None)
        })?),
        None => None,
    };
    Ok(ScopeSpec {
        user_id: p.user_id.clone(),
        agent_id: p.agent_id.clone(),
        session_id,
        project_id: p.project_id,
    })
}

// ----------------------------------------------------------------------------
// memory_create_entities
// ----------------------------------------------------------------------------

const MAX_CREATE_ENTITIES_BATCH: usize = 100;
const MAX_ENTITY_FIELD_BYTES: usize = 256;
const MAX_OBSERVATIONS_PER_ENTITY: usize = 64;
const MAX_OBSERVATION_BYTES: usize = 16 * 1024;

fn normalize_entity_field(value: String, field: &str) -> Result<String, McpError> {
    let normalized = value.trim();
    if normalized.is_empty() {
        return Err(McpError::invalid_params(
            format!("{field} must not be blank"),
            None,
        ));
    }
    if normalized.len() > MAX_ENTITY_FIELD_BYTES {
        return Err(McpError::invalid_params(
            format!("{field} must be at most {MAX_ENTITY_FIELD_BYTES} bytes"),
            None,
        ));
    }
    Ok(normalized.to_string())
}

fn validate_initial_observations(observations: Vec<String>) -> Result<Vec<String>, McpError> {
    if observations.len() > MAX_OBSERVATIONS_PER_ENTITY {
        return Err(McpError::invalid_params(
            format!(
                "observations must contain at most {MAX_OBSERVATIONS_PER_ENTITY} entries per entity"
            ),
            None,
        ));
    }

    let mut out = Vec::with_capacity(observations.len());
    for obs in observations {
        if obs.trim().is_empty() {
            return Err(McpError::invalid_params(
                "observations must not contain blank entries",
                None,
            ));
        }
        if obs.len() > MAX_OBSERVATION_BYTES {
            return Err(McpError::invalid_params(
                format!("observations must be at most {MAX_OBSERVATION_BYTES} bytes each"),
                None,
            ));
        }
        out.push(obs);
    }
    Ok(out)
}

pub async fn tool_memory_create_entities(
    ctx: &SystemContext,
    params: MemoryCreateEntitiesParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "memory_create_entities", "MCP tool invoked");
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    let scope = parse_scope(params.scope.as_ref())?;

    if params.entities.is_empty() {
        return Err(McpError::invalid_params("entities must not be empty", None));
    }
    if params.entities.len() > MAX_CREATE_ENTITIES_BATCH {
        return Err(McpError::invalid_params(
            format!("entities must contain at most {MAX_CREATE_ENTITIES_BATCH} entries"),
            None,
        ));
    }

    let inputs: Vec<NewEntityInput> = params
        .entities
        .into_iter()
        .map(|e| {
            Ok(NewEntityInput {
                name: normalize_entity_field(e.name, "entity name")?,
                entity_type: normalize_entity_field(e.entity_type, "entity_type")?,
                observations: validate_initial_observations(e.observations.unwrap_or_default())?,
            })
        })
        .collect::<Result<_, McpError>>()?;

    let scope_id = queries::find_or_create_scope(pool, &scope)
        .await
        .map_err(|e| McpError::internal_error(format!("scope: {}", e), None))?;

    let created = queries::memory_create_entities_detailed(pool, &inputs, scope_id, "agent_write")
        .await
        .map_err(|e| {
            error!(tool = "memory_create_entities", error = %e, "query failed");
            match &e {
                sqlx::Error::Protocol(msg) => McpError::invalid_params(msg.clone(), None),
                _ => McpError::internal_error(format!("query failed: {}", e), None),
            }
        })?;

    ctx.stats()
        .memory_entities_created
        .fetch_add(created.entities_inserted as u64, Ordering::Relaxed);
    ctx.stats()
        .memory_observations_added
        .fetch_add(created.observations_inserted as u64, Ordering::Relaxed);

    debug!(
        tool = "memory_create_entities",
        scope_id,
        count = created.entity_ids.len(),
        entities_inserted = created.entities_inserted,
        observations_added = created.observations_inserted,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );
    json_result(json!({
        "scope_id": scope_id,
        "entities_created": created.entities_inserted,
        "entities_processed": created.entity_ids.len(),
        "ids": created.entity_ids,
        "observations_attached": created.observations_inserted,
    }))
}

// ----------------------------------------------------------------------------
// memory_create_relations
// ----------------------------------------------------------------------------

const MAX_CREATE_RELATIONS_BATCH: usize = 500;

fn normalize_relation_field(value: String, field: &str) -> Result<String, McpError> {
    normalize_entity_field(value, field)
}

pub async fn tool_memory_create_relations(
    ctx: &SystemContext,
    params: MemoryCreateRelationsParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "memory_create_relations", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    if params.relations.is_empty() {
        return Err(McpError::invalid_params(
            "relations must not be empty",
            None,
        ));
    }
    if params.relations.len() > MAX_CREATE_RELATIONS_BATCH {
        return Err(McpError::invalid_params(
            format!("relations must contain at most {MAX_CREATE_RELATIONS_BATCH} entries"),
            None,
        ));
    }
    let inputs: Vec<NewRelationInput> = params
        .relations
        .into_iter()
        .map(|r| {
            Ok(NewRelationInput {
                from: normalize_relation_field(r.from, "relation from")?,
                to: normalize_relation_field(r.to, "relation to")?,
                relation_type: normalize_relation_field(r.relation_type, "relation_type")?,
            })
        })
        .collect::<Result<_, McpError>>()?;
    let created = queries::memory_create_relations_detailed(pool, &inputs, "agent_write")
        .await
        .map_err(|e| match &e {
            sqlx::Error::Protocol(msg) => McpError::invalid_params(msg.clone(), None),
            _ => McpError::internal_error(format!("query failed: {}", e), None),
        })?;
    let resolved = created.relation_ids.iter().filter(|i| **i >= 0).count();
    let unresolved = created.relation_ids.iter().filter(|i| **i < 0).count();
    ctx.stats()
        .memory_relations_created
        .fetch_add(created.relations_inserted as u64, Ordering::Relaxed);
    json_result(json!({
        "relations_created": created.relations_inserted,
        "relations_resolved": resolved,
        "unresolved_endpoints": unresolved,
        "ids": created.relation_ids,
    }))
}

// ----------------------------------------------------------------------------
// memory_add_observations
// ----------------------------------------------------------------------------

pub async fn tool_memory_add_observations(
    ctx: &SystemContext,
    params: MemoryAddObservationsParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "memory_add_observations", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    if params.observations.is_empty() {
        return Err(McpError::invalid_params(
            "observations must not be empty",
            None,
        ));
    }
    let inputs: Vec<AddObservationInput> = params
        .observations
        .into_iter()
        .map(|o| AddObservationInput {
            entity_name: o.entity_name,
            contents: o.contents,
        })
        .collect();
    let ids = queries::memory_add_observations(pool, &inputs, "agent_write")
        .await
        .map_err(|e| match &e {
            sqlx::Error::Protocol(msg) => McpError::invalid_params(msg.clone(), None),
            _ => McpError::internal_error(format!("query failed: {}", e), None),
        })?;
    ctx.stats()
        .memory_observations_added
        .fetch_add(ids.len() as u64, Ordering::Relaxed);
    json_result(json!({
        "observations_added": ids.len(),
        "ids": ids,
    }))
}

// ----------------------------------------------------------------------------
// memory_delete_entities (soft delete)
// ----------------------------------------------------------------------------

pub async fn tool_memory_delete_entities(
    ctx: &SystemContext,
    params: MemoryDeleteEntitiesParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "memory_delete_entities", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    if params.names.is_empty() {
        return Err(McpError::invalid_params("names must not be empty", None));
    }
    let affected = queries::memory_delete_entities(pool, &params.names)
        .await
        .map_err(|e| McpError::internal_error(format!("query failed: {}", e), None))?;
    ctx.stats()
        .memory_entities_deleted
        .fetch_add(affected, Ordering::Relaxed);
    json_result(json!({
        "soft_deleted": affected,
        "names": params.names,
        "mode": "soft_delete_via_valid_to",
    }))
}

// ----------------------------------------------------------------------------
// memory_delete_observations (soft delete)
// ----------------------------------------------------------------------------

pub async fn tool_memory_delete_observations(
    ctx: &SystemContext,
    params: MemoryDeleteObservationsParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "memory_delete_observations", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    if params.deletions.is_empty() {
        return Err(McpError::invalid_params(
            "deletions must not be empty",
            None,
        ));
    }
    let inputs: Vec<DeleteObservationInput> = params
        .deletions
        .into_iter()
        .map(|d| DeleteObservationInput {
            entity_name: d.entity_name,
            observations: d.observations,
        })
        .collect();
    let affected = queries::memory_delete_observations(pool, &inputs)
        .await
        .map_err(|e| McpError::internal_error(format!("query failed: {}", e), None))?;
    ctx.stats()
        .memory_observations_deleted
        .fetch_add(affected, Ordering::Relaxed);
    json_result(json!({
        "soft_deleted": affected,
        "mode": "soft_delete_via_valid_to",
    }))
}

// ----------------------------------------------------------------------------
// memory_delete_relations (soft delete)
// ----------------------------------------------------------------------------

pub async fn tool_memory_delete_relations(
    ctx: &SystemContext,
    params: MemoryDeleteRelationsParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "memory_delete_relations", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    if params.relations.is_empty() {
        return Err(McpError::invalid_params(
            "relations must not be empty",
            None,
        ));
    }
    let inputs: Vec<NewRelationInput> = params
        .relations
        .into_iter()
        .map(|r| NewRelationInput {
            from: r.from,
            to: r.to,
            relation_type: r.relation_type,
        })
        .collect();
    let affected = queries::memory_delete_relations(pool, &inputs)
        .await
        .map_err(|e| McpError::internal_error(format!("query failed: {}", e), None))?;
    ctx.stats()
        .memory_relations_deleted
        .fetch_add(affected, Ordering::Relaxed);
    json_result(json!({
        "soft_deleted": affected,
        "mode": "soft_delete_via_valid_to",
    }))
}

// ----------------------------------------------------------------------------
// memory_read_graph
// ----------------------------------------------------------------------------

pub async fn tool_memory_read_graph(
    ctx: &SystemContext,
    params: MemoryReadGraphParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "memory_read_graph", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .memory_read_graph_calls
        .fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    let scope_id = match params.scope.as_ref() {
        Some(scope) => {
            let spec = parse_scope(Some(scope))?;
            Some(
                queries::find_or_create_scope(pool, &spec)
                    .await
                    .map_err(|e| McpError::internal_error(format!("scope: {}", e), None))?,
            )
        }
        None => None,
    };
    let limit = params.limit_entities.unwrap_or(200);
    let dump = queries::memory_read_graph(pool, scope_id, limit)
        .await
        .map_err(|e| McpError::internal_error(format!("query failed: {}", e), None))?;
    json_result(json!(dump))
}

// ----------------------------------------------------------------------------
// memory_search_nodes
// ----------------------------------------------------------------------------

pub async fn tool_memory_search_nodes(
    ctx: &SystemContext,
    params: MemorySearchNodesParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "memory_search_nodes", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .memory_search_nodes_calls
        .fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    if params.query.trim().is_empty() {
        return Err(McpError::invalid_params("query must not be empty", None));
    }
    let scope_id = match params.scope.as_ref() {
        Some(scope) => {
            let spec = parse_scope(Some(scope))?;
            Some(
                queries::find_or_create_scope(pool, &spec)
                    .await
                    .map_err(|e| McpError::internal_error(format!("scope: {}", e), None))?,
            )
        }
        None => None,
    };
    let limit = params.limit.unwrap_or(20);
    let hits = queries::memory_search_nodes(pool, &params.query, scope_id, limit)
        .await
        .map_err(|e| McpError::internal_error(format!("query failed: {}", e), None))?;
    json_result(json!({
        "count": hits.len(),
        "results": hits,
    }))
}

// ----------------------------------------------------------------------------
// memory_open_nodes
// ----------------------------------------------------------------------------

pub async fn tool_memory_open_nodes(
    ctx: &SystemContext,
    params: MemoryOpenNodesParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "memory_open_nodes", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .memory_open_nodes_calls
        .fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    let names = normalize_open_node_names(params.names)?;
    let nodes = queries::memory_open_nodes(pool, &names)
        .await
        .map_err(|e| McpError::internal_error(format!("query failed: {}", e), None))?;
    // Shadow-ASR channel (Phase D2b): project-scoped effect distribution.
    let pid =
        crate::mcp::tools::sema_helpers::effects::project_id_opt(pool, params.project.as_deref())
            .await;
    let effect_breakdown =
        crate::mcp::tools::sema_helpers::effects::effect_breakdown_json(pool, pid).await;

    json_result(json!({
        "effect_breakdown": effect_breakdown,
        "requested_names": names,
        "name_cap": MEMORY_OPEN_NODES_MAX_NAMES,
        "count": nodes.len(),
        "nodes": nodes,
    }))
}
