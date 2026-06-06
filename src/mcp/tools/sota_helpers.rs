//! Shared helpers for SOTA Phase 2-11 MCP tool bodies.
//!
//! Most tools follow the same shape: look up project_id, load the graph (or
//! query a derived metric), run the algorithm, return JSON. This module
//! exposes the common scaffolding so each tool file stays small.

#![allow(dead_code)]

use std::collections::HashMap;

use petgraph::graph::{DiGraph, NodeIndex};
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use sqlx::PgPool;

use crate::context::SystemContext;

/// Look up `projects.id` by display name; returns a McpError if not found.
/// Duplicate names fail closed because most SOTA tools scope every downstream
/// query by this id and would otherwise report on an arbitrary project.
pub async fn project_id_or_err(ctx: &SystemContext, project: &str) -> Result<i32, McpError> {
    let pool = pool_or_err(ctx)?;
    let project = project.trim();
    if project.is_empty() {
        return Err(McpError::invalid_params("project must be non-empty", None));
    }
    let ids: Vec<i32> = sqlx::query_scalar("SELECT id FROM projects WHERE name = $1 ORDER BY id")
        .bind(project)
        .fetch_all(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("Project lookup failed: {}", e), None))?;
    match ids.as_slice() {
        [] => Err(McpError::internal_error(
            format!("Project not found: {}", project),
            None,
        )),
        [id] => Ok(*id),
        ids => Err(McpError::invalid_params(
            format!(
                "ambiguous project name '{}' matched {} indexed projects; use a unique project name from list_projects",
                project,
                ids.len()
            ),
            None,
        )),
    }
}

/// Get the pool from the DbClient or error.
pub fn pool_or_err(ctx: &SystemContext) -> Result<&PgPool, McpError> {
    ctx.db().pool().ok_or_else(|| {
        McpError::internal_error(
            "Inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>",
            None,
        )
    })
}

/// Wrap a serializable result as a CallToolResult text content.
pub fn json_result<T: serde::Serialize>(v: &T) -> Result<CallToolResult, McpError> {
    let json = serde_json::to_string_pretty(v)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

/// Convenience text result.
pub fn text_result(s: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![Content::text(s.into())])
}

/// Count files that participate in an import cycle for one already-resolved
/// project id. Uses Tarjan SCC over the materialized import graph, so detection
/// is O(files + edges) and catches cycles of any length without recursive SQL
/// path explosion.
pub async fn import_cycle_file_count(pool: &PgPool, project_id: i32) -> Result<i64, sqlx::Error> {
    #[derive(sqlx::FromRow)]
    struct ImportEdge {
        source_file_id: i64,
        target_file_id: i64,
    }

    let edges: Vec<ImportEdge> = sqlx::query_as(
        "SELECT source_file_id, target_file_id
         FROM code_graph_edges
         WHERE project_id = $1 AND edge_type = 'import' AND target_file_id IS NOT NULL",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await?;

    if edges.is_empty() {
        return Ok(0);
    }

    let mut graph = DiGraph::<(), ()>::new();
    let mut nodes: HashMap<i64, NodeIndex> = HashMap::with_capacity(edges.len() * 2);
    for edge in edges {
        let source = *nodes
            .entry(edge.source_file_id)
            .or_insert_with(|| graph.add_node(()));
        let target = *nodes
            .entry(edge.target_file_id)
            .or_insert_with(|| graph.add_node(()));
        if source != target {
            graph.add_edge(source, target, ());
        }
    }

    Ok(crate::graph::algorithms::find_cycles(&graph)
        .into_iter()
        .map(|scc| scc.len() as i64)
        .sum())
}
