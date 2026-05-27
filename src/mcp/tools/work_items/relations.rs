//! Relations + code-anchor tools (Phase 9).
//!
//! `work_item_link`/`work_item_unlink` manage the typed `item_relations` DAG
//! that is orthogonal to the `parent_id` tree. The two *ordering* relations —
//! `depends_on` and `blocks` — must stay acyclic (an unschedulable loop is
//! rejected at link time via a petgraph cycle check). `work_item_cycles`
//! reports any cycles that already exist (e.g. from a bulk import that bypassed
//! the per-edge guard). `work_item_anchor_code` ties an item to a code location
//! (file / chunk / symbol) for the auditor + change-impact surfaces.

use std::collections::HashMap;
use std::sync::atomic::Ordering;

use petgraph::algo::{is_cyclic_directed, tarjan_scc};
use petgraph::graph::{DiGraph, NodeIndex};
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::{Value, json};

use crate::context::SystemContext;
use crate::db::queries;
use crate::mcp::server::{
    WorkItemAnchorCodeParams, WorkItemCyclesParams, WorkItemLinkParams, WorkItemUnlinkParams,
};
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};
use crate::mcp::tools::work_items::crud::{id_of_public, map_db_err};

/// The closed `item_relations.relation_type` vocabulary (mirrors the DB CHECK;
/// validated here for a clean error rather than a raw constraint violation).
const RELATION_TYPES: &[&str] = &[
    "blocks",
    "depends_on",
    "relates_to",
    "duplicates",
    "supersedes",
    "derived_from",
];

/// Get-or-insert a node for `id`, returning its index.
fn node_for(g: &mut DiGraph<i64, ()>, idx: &mut HashMap<i64, NodeIndex>, id: i64) -> NodeIndex {
    *idx.entry(id).or_insert_with(|| g.add_node(id))
}

/// Build the must-precede graph from `edges` (`(pre, post)` pairs).
fn build_constraint_graph(edges: &[(i64, i64)]) -> (DiGraph<i64, ()>, HashMap<i64, NodeIndex>) {
    let mut g: DiGraph<i64, ()> = DiGraph::new();
    let mut idx: HashMap<i64, NodeIndex> = HashMap::with_capacity(edges.len() * 2);
    for &(pre, post) in edges {
        let a = node_for(&mut g, &mut idx, pre);
        let b = node_for(&mut g, &mut idx, post);
        g.add_edge(a, b, ());
    }
    (g, idx)
}

/// Map a proposed `relation_type` link `from → to` to its must-precede edge
/// `(pre, post)`. Only defined for the ordering relations.
fn ordering_edge(relation_type: &str, from_id: i64, to_id: i64) -> Option<(i64, i64)> {
    match relation_type {
        // "from depends_on to" ⇒ to must precede from.
        "depends_on" => Some((to_id, from_id)),
        // "from blocks to" ⇒ from must precede to.
        "blocks" => Some((from_id, to_id)),
        _ => None,
    }
}

/// `work_item_link` — create a typed relation `from --(relation_type)--> to`.
/// For the ordering relations (`depends_on`/`blocks`) the edge is rejected if it
/// would close a cycle in the combined schedule graph.
pub async fn tool_work_item_link(
    ctx: &SystemContext,
    params: WorkItemLinkParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let relation_type = params.relation_type.trim();
    if !RELATION_TYPES.contains(&relation_type) {
        return Err(McpError::invalid_params(
            format!("unknown relation_type '{relation_type}'; expected one of {RELATION_TYPES:?}"),
            None,
        ));
    }

    let from_id = id_of_public(pool, &params.from_public_id).await?;
    let to_id = id_of_public(pool, &params.to_public_id).await?;
    if from_id == to_id {
        return Err(McpError::invalid_params(
            "cannot relate an item to itself",
            None,
        ));
    }

    // Cycle guard for ordering relations: build the existing must-precede graph,
    // add the proposed edge, and reject if the result is cyclic.
    if let Some((pre, post)) = ordering_edge(relation_type, from_id, to_id) {
        // Workspace-wide: a dependency cycle is unschedulable regardless of
        // which plan(s) the items belong to.
        let edges = queries::fetch_constraint_edges(pool, None)
            .await
            .map_err(map_db_err)?;
        let (mut g, mut idx) = build_constraint_graph(&edges);
        let a = node_for(&mut g, &mut idx, pre);
        let b = node_for(&mut g, &mut idx, post);
        g.add_edge(a, b, ());
        if is_cyclic_directed(&g) {
            return Err(McpError::invalid_params(
                format!(
                    "linking '{}' {} '{}' would create a dependency cycle (unschedulable); \
                     resolve the existing chain first or use work_item_cycles to inspect it",
                    params.from_public_id, relation_type, params.to_public_id
                ),
                None,
            ));
        }
    }

    let agent = params.created_by.as_deref().filter(|s| !s.is_empty());
    let relation_id = queries::insert_relation(pool, from_id, to_id, relation_type, agent)
        .await
        .map_err(map_db_err)?;

    json_result(&json!({
        "linked": true,
        "relation_id": relation_id,
        "from": params.from_public_id,
        "to": params.to_public_id,
        "relation_type": relation_type,
    }))
}

/// `work_item_unlink` — remove a typed relation. Returns `{removed: bool}`
/// (false if the relation did not exist).
pub async fn tool_work_item_unlink(
    ctx: &SystemContext,
    params: WorkItemUnlinkParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let relation_type = params.relation_type.trim();
    if !RELATION_TYPES.contains(&relation_type) {
        return Err(McpError::invalid_params(
            format!("unknown relation_type '{relation_type}'"),
            None,
        ));
    }
    let from_id = id_of_public(pool, &params.from_public_id).await?;
    let to_id = id_of_public(pool, &params.to_public_id).await?;
    let removed = queries::delete_relation(pool, from_id, to_id, relation_type)
        .await
        .map_err(map_db_err)?;
    json_result(&json!({ "removed": removed }))
}

/// `work_item_cycles` — report dependency cycles in the schedule graph
/// (`depends_on` + `blocks`). Each reported cycle is a strongly-connected
/// component of size > 1; an empty report means the schedule is a valid DAG.
pub async fn tool_work_item_cycles(
    ctx: &SystemContext,
    params: WorkItemCyclesParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .work_item_queries
        .fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    // Optionally scope the report to one plan's subtree.
    let root_id = match params.plan_public_id.as_deref().filter(|s| !s.is_empty()) {
        Some(p) => Some(id_of_public(pool, p).await?),
        None => None,
    };
    let edges = queries::fetch_constraint_edges(pool, root_id)
        .await
        .map_err(map_db_err)?;
    let (g, _idx) = build_constraint_graph(&edges);

    // Tarjan SCCs; any component with > 1 node is a cycle (self-loops are
    // impossible — the table CHECK forbids from == to).
    let sccs: Vec<Vec<i64>> = tarjan_scc(&g)
        .into_iter()
        .filter(|c| c.len() > 1)
        .map(|c| c.into_iter().map(|n| g[n]).collect())
        .collect();

    // Resolve the involved ids to public_id/title/status for a legible report.
    let all_ids: Vec<i64> = sccs.iter().flatten().copied().collect();
    let meta = queries::fetch_items_meta(pool, &all_ids)
        .await
        .map_err(map_db_err)?;
    let by_id: HashMap<i64, &queries::ItemMeta> = meta.iter().map(|m| (m.id, m)).collect();

    let cycles: Vec<Value> = sccs
        .iter()
        .map(|c| {
            let members: Vec<Value> = c
                .iter()
                .map(|id| match by_id.get(id) {
                    Some(m) => json!({
                        "public_id": m.public_id,
                        "title": m.title,
                        "kind": m.kind,
                        "status": m.status,
                    }),
                    None => json!({ "id": id }),
                })
                .collect();
            json!({ "size": c.len(), "members": members })
        })
        .collect();

    json_result(&json!({
        "cycle_count": cycles.len(),
        "is_dag": cycles.is_empty(),
        "cycles": cycles,
    }))
}

/// `work_item_anchor_code` — tie an item to a code location. Accepts a file
/// path (resolved to `indexed_files.id`) and/or explicit `chunk_id`/`symbol_id`;
/// at least one must resolve.
pub async fn tool_work_item_anchor_code(
    ctx: &SystemContext,
    params: WorkItemAnchorCodeParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let item_id = id_of_public(pool, &params.public_id).await?;

    // Resolve a path → file_id if one was given.
    let file_id = match params.file.as_deref().filter(|s| !s.is_empty()) {
        None => None,
        Some(path) => {
            let id = queries::resolve_file_id_by_path(pool, path)
                .await
                .map_err(map_db_err)?
                .ok_or_else(|| {
                    McpError::invalid_params(format!("no indexed file matches '{path}'"), None)
                })?;
            Some(id)
        }
    };
    let chunk_id = params.chunk_id;
    let symbol_id = params.symbol_id;

    if file_id.is_none() && chunk_id.is_none() && symbol_id.is_none() {
        return Err(McpError::invalid_params(
            "provide at least one of: file (path), chunk_id, symbol_id",
            None,
        ));
    }
    // Default anchor_type by the most specific reference provided.
    let anchor_type = params
        .anchor_type
        .as_deref()
        .unwrap_or(if symbol_id.is_some() {
            "symbol"
        } else if chunk_id.is_some() {
            "chunk"
        } else {
            "file"
        });

    let anchor_id =
        queries::insert_code_anchor(pool, item_id, file_id, chunk_id, symbol_id, anchor_type)
            .await
            .map_err(map_db_err)?;

    json_result(&json!({
        "anchored": true,
        "anchor_id": anchor_id,
        "public_id": params.public_id,
        "file_id": file_id,
        "chunk_id": chunk_id,
        "symbol_id": symbol_id,
        "anchor_type": anchor_type,
    }))
}
