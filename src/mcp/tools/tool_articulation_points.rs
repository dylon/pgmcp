//! `tool_articulation_points` — cut vertices + bridges (graph-roadmap Phase 2.6).
//!
//! Hopcroft-Tarjan articulation points and bridges over the file import graph
//! or the function call graph: true structural single points of failure
//! (nodes whose removal disconnects the graph) and irreplaceable dependencies
//! (edges with no alternate path). Sharper than the ownership-based
//! `bus_factor`, which is about *who* knows the code, not *what* the structure
//! depends on.

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::graph::algorithms::compute_degrees;
use crate::graph::algorithms_ext::articulation_points_and_bridges;
use crate::mcp::server::ArticulationPointsParams;
use crate::mcp::tools::graph_scope::load_scoped_graph;
use crate::mcp::tools::sota_helpers::{json_result, project_id_or_err};

pub async fn tool_articulation_points(
    ctx: &SystemContext,
    params: ArticulationPointsParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "articulation_points", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let scope = params.scope.as_deref().unwrap_or("file");
    let limit = params.limit.unwrap_or(100).clamp(1, 2000) as usize;

    let g = load_scoped_graph(ctx, project_id, scope).await?;
    let cut = articulation_points_and_bridges(&g);
    let degrees = compute_degrees(&g);

    // Rank cut vertices by total degree (high-degree SPOFs are the worst).
    let mut cuts: Vec<_> = cut.articulation_points.iter().copied().collect();
    cuts.sort_by(|a, b| {
        let da = degrees.get(a).map(|(i, o)| i + o).unwrap_or(0);
        let db = degrees.get(b).map(|(i, o)| i + o).unwrap_or(0);
        db.cmp(&da).then(a.index().cmp(&b.index()))
    });
    let articulation: Vec<_> = cuts
        .iter()
        .take(limit)
        .filter_map(|&ni| {
            g.node_weight(ni).map(|m| {
                let (i, o) = degrees.get(&ni).copied().unwrap_or((0, 0));
                let mut obj = m.to_json();
                if let Some(map) = obj.as_object_mut() {
                    map.insert("degree".into(), json!(i + o));
                }
                obj
            })
        })
        .collect();

    let bridges: Vec<_> = cut
        .bridges
        .iter()
        .take(limit)
        .filter_map(|(a, b)| match (g.node_weight(*a), g.node_weight(*b)) {
            (Some(na), Some(nb)) => Some(json!({ "from": na.to_json(), "to": nb.to_json() })),
            _ => None,
        })
        .collect();

    json_result(&json!({
        "project": params.project,
        "scope": scope,
        "articulation_point_count": cut.articulation_points.len(),
        "bridge_count": cut.bridges.len(),
        "articulation_points": articulation,
        "bridges": bridges,
        "guidance": "Articulation points are nodes whose removal disconnects the dependency graph \
            (computed on the undirected projection) — true structural single points of failure, ranked \
            here by degree. Bridges are edges whose removal disconnects it: irreplaceable dependencies \
            with no alternate path. Harden high-degree articulation points (tests, docs, ownership, \
            interface seams) and consider redundancy or an abstraction boundary around a bridge into a \
            critical subsystem. Complements `bus_factor` (who knows it) with what the structure hinges on."
    }))
}
