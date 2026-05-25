//! `tool_dominator_tree` — chokepoints via the dominator tree (Phase 2.6).
//!
//! From a chosen root (entry point), computes the dominator tree and ranks
//! nodes by how many others they dominate — the "must-pass-through" chokepoints
//! every path from the root has to traverse. Natural on the call graph rooted
//! at `main`/a handler; on the import graph it finds architectural funnels.

use std::collections::HashMap;
use std::sync::atomic::Ordering;

use petgraph::graph::NodeIndex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::graph::algorithms::compute_degrees;
use crate::graph::algorithms_ext::dominator_tree;
use crate::mcp::server::DominatorTreeParams;
use crate::mcp::tools::graph_scope::load_scoped_graph;
use crate::mcp::tools::sota_helpers::{json_result, project_id_or_err};

pub async fn tool_dominator_tree(
    ctx: &SystemContext,
    params: DominatorTreeParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "dominator_tree", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let scope = params.scope.as_deref().unwrap_or("file");
    let limit = params.limit.unwrap_or(50).clamp(1, 1000) as usize;

    let g = load_scoped_graph(ctx, project_id, scope).await?;
    if g.node_count() == 0 {
        return json_result(&json!({
            "project": params.project, "scope": scope, "chokepoints": [],
            "guidance": "Empty graph — nothing to analyze."
        }));
    }

    // Resolve the root: explicit `root` (exact label, else substring), or the
    // max-out-degree node as a heuristic entry point / orchestrator.
    let root_ni: NodeIndex = match params.root.as_deref() {
        Some(r) => g
            .node_indices()
            .find(|&ni| g.node_weight(ni).map(|m| m.label == r).unwrap_or(false))
            .or_else(|| {
                g.node_indices().find(|&ni| {
                    g.node_weight(ni)
                        .map(|m| m.label.contains(r))
                        .unwrap_or(false)
                })
            })
            .ok_or_else(|| {
                McpError::invalid_params(format!("root '{r}' not found in the {scope} graph"), None)
            })?,
        None => {
            let degrees = compute_degrees(&g);
            g.node_indices()
                .max_by_key(|ni| degrees.get(ni).map(|(_, o)| *o).unwrap_or(0))
                .expect("non-empty graph has a node")
        }
    };

    let idom = dominator_tree(&g, root_ni);

    // dominated[d] = number of nodes d strictly dominates (its dominator-tree
    // subtree minus itself): removing d cuts that many nodes off from the root.
    let mut dominated: HashMap<NodeIndex, usize> = HashMap::with_capacity(idom.len());
    for &v in idom.keys() {
        if v == root_ni {
            continue;
        }
        let mut cur = idom[&v];
        loop {
            *dominated.entry(cur).or_insert(0) += 1;
            if cur == root_ni {
                break;
            }
            cur = idom[&cur];
        }
    }

    let mut ranked: Vec<_> = dominated.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.index().cmp(&b.0.index())));
    let chokepoints: Vec<_> = ranked
        .iter()
        .take(limit)
        .filter_map(|(ni, count)| {
            g.node_weight(*ni).map(|m| {
                let mut obj = m.to_json();
                if let Some(map) = obj.as_object_mut() {
                    map.insert("dominates".into(), json!(count));
                }
                obj
            })
        })
        .collect();

    let root_label = g.node_weight(root_ni).map(|m| m.to_json());

    json_result(&json!({
        "project": params.project,
        "scope": scope,
        "root": root_label,
        "reachable_nodes": idom.len(),
        "chokepoints": chokepoints,
        "guidance": "From the chosen root, each chokepoint's `dominates` count is how many reachable \
            nodes have it as a dominator — i.e. every path from the root to those nodes must pass \
            through it. High-`dominates` nodes are mandatory funnels: removing/breaking one severs the \
            whole subtree from the entry point, and they're the highest-leverage places to add caching, \
            validation, or an interface boundary. Pick `root` to match the entry point you care about \
            (a handler, `main`, a public API); the default is the highest-out-degree node."
    }))
}
