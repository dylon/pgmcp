//! `tool_graph_connectivity` — robustness & decoupling structure (graph-roadmap
//! Phase 3.6).
//!
//! Three views over the file import graph or the function call graph:
//! - **2-edge-connected components** (Tarjan 1972): subsystems that survive any
//!   single edge removal; size-1 components are single-edge points of failure.
//! - **Global min-cut** (Stoer-Wagner, JACM 1997): the weakest seam — the
//!   minimum-weight edge set whose removal splits the graph in two — a concrete
//!   module-decoupling boundary.
//! - **Well-connected communities** (Leiden refinement, Traag et al. 2019):
//!   Louvain communities split so none is internally disconnected, with a count
//!   of how many Louvain communities the refinement had to break apart.

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::graph::algorithms::louvain_communities;
use crate::graph::connectivity::{
    global_min_cut, refine_communities_connected, two_edge_connected_components,
};
use crate::mcp::server::GraphConnectivityParams;
use crate::mcp::tools::graph_scope::load_scoped_graph;
use crate::mcp::tools::sota_helpers::{json_result, project_id_or_err};

/// Stoer-Wagner is O(V³); skip the global min-cut above this many nodes.
const MAX_MINCUT_NODES: usize = 2000;

pub async fn tool_graph_connectivity(
    ctx: &SystemContext,
    params: GraphConnectivityParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "graph_connectivity", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let scope = params.scope.as_deref().unwrap_or("file");
    let limit = params.limit.unwrap_or(20).clamp(1, 500) as usize;

    let g = load_scoped_graph(ctx, project_id, scope).await?;
    let n = g.node_count();
    if n == 0 {
        return json_result(&json!({
            "project": params.project,
            "scope": scope,
            "node_count": 0,
            "guidance": "Empty graph — run symbol-extraction / graph-analysis crons first."
        }));
    }

    // 2-edge-connected components.
    let comps = two_edge_connected_components(&g);
    let singletons = comps.iter().filter(|c| c.len() == 1).count();
    let comp_json: Vec<serde_json::Value> = comps
        .iter()
        .take(limit)
        .map(|c| {
            let members: Vec<&str> = c
                .iter()
                .take(12)
                .filter_map(|&ni| g.node_weight(ni).map(|m| m.label.as_str()))
                .collect();
            json!({ "size": c.len(), "members": members })
        })
        .collect();

    // Global min-cut (gated on node count).
    let mincut = if n <= MAX_MINCUT_NODES {
        global_min_cut(&g).map(|c| {
            let side: Vec<&str> = c
                .partition
                .iter()
                .take(limit)
                .filter_map(|&ni| g.node_weight(ni).map(|m| m.label.as_str()))
                .collect();
            json!({
                "weight": format!("{:.3}", c.weight),
                "partition_size": c.partition.len(),
                "other_size": n - c.partition.len(),
                "partition_sample": side,
            })
        })
    } else {
        None
    };

    // Leiden well-connectedness refinement of Louvain.
    let louvain = louvain_communities(&g, 1.0);
    let (_refined, refined_k) = refine_communities_connected(&g, &louvain.communities);
    let split_count = refined_k.saturating_sub(louvain.num_communities);

    json_result(&json!({
        "project": params.project,
        "scope": scope,
        "node_count": n,
        "two_edge_connected": {
            "component_count": comps.len(),
            "single_points_of_edge_failure": singletons,
            "components": comp_json,
        },
        "global_min_cut": mincut,
        "min_cut_skipped_large_graph": n > MAX_MINCUT_NODES,
        "communities": {
            "louvain_count": louvain.num_communities,
            "well_connected_count": refined_k,
            "louvain_communities_split": split_count,
            "modularity": format!("{:.4}", louvain.modularity),
        },
        "guidance": "2-edge-connected components are subsystems that survive any single dependency \
            removal; size-1 = a single-edge point of failure (harden or add redundancy). `global_min_cut` \
            is the weakest seam — the minimum-weight edge set whose removal splits the graph; its two sides \
            are a concrete module-decoupling boundary (feed `recommend_module_split`). \
            `louvain_communities_split` counts how many Louvain communities were internally disconnected \
            and had to be broken apart by the Leiden well-connectedness refinement — a nonzero value means \
            plain Louvain was over-merging across a structural gap."
    }))
}
