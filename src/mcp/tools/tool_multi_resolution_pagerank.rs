//! `tool_multi_resolution_pagerank` — PageRank within communities + on the
//! community-supernode graph (SOTA Phase 8.4).

#![allow(unused_imports)]

use std::collections::HashMap;
use std::sync::atomic::Ordering;

use petgraph::graph::{DiGraph, NodeIndex};
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::graph::algorithms::{louvain_communities, pagerank};
use crate::graph::types::{EdgeWeight, FileNode};
use crate::mcp::server::MultiResolutionPagerankParams;
use crate::mcp::tools::fix_helpers::load_import_graph;
use crate::mcp::tools::sota_helpers::{json_result, project_id_or_err};

pub async fn tool_multi_resolution_pagerank(
    ctx: &SystemContext,
    params: MultiResolutionPagerankParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "multi_resolution_pagerank", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let bundle = load_import_graph(ctx, project_id).await?;
    let limit = params.limit.unwrap_or(50);

    let louv = louvain_communities(&bundle.graph, 1.0);
    let global_pr = pagerank(&bundle.graph.graph, 0.85, 100, 1e-6);

    // Build per-community induced subgraphs and run PageRank within each.
    let mut intra: HashMap<NodeIndex, f64> = HashMap::new();
    let comm_for_node: HashMap<NodeIndex, usize> = louv.communities.clone();
    // Invert node->community map into community->members.
    let mut members_by_cid: HashMap<usize, Vec<NodeIndex>> = HashMap::new();
    for (&ni, &cid) in &louv.communities {
        members_by_cid.entry(cid).or_default().push(ni);
    }
    let total_communities = members_by_cid.len();
    for (cid, members) in &members_by_cid {
        let member_set: std::collections::HashSet<NodeIndex> = members.iter().copied().collect();
        // Build induced subgraph copy.
        let mut sub: DiGraph<FileNode, EdgeWeight> = DiGraph::new();
        let mut idx_map: HashMap<NodeIndex, NodeIndex> = HashMap::new();
        for &m in &member_set {
            if let Some(node) = bundle.graph.graph.node_weight(m).cloned() {
                let new_ni = sub.add_node(node);
                idx_map.insert(m, new_ni);
            }
        }
        for e in bundle.graph.graph.edge_indices() {
            if let Some((s, t)) = bundle.graph.graph.edge_endpoints(e)
                && let (Some(&ns), Some(&nt)) = (idx_map.get(&s), idx_map.get(&t))
                && let Some(w) = bundle.graph.graph.edge_weight(e).cloned()
            {
                sub.add_edge(ns, nt, w);
            }
        }
        let sub_pr = pagerank(&sub, 0.85, 100, 1e-6);
        for (orig, sub_ni) in idx_map.iter() {
            let s = sub_pr.scores.get(sub_ni).copied().unwrap_or(0.0);
            intra.insert(*orig, s);
        }
        let _ = cid;
    }

    // Inter-community: supernode PageRank.
    let mut super_g: DiGraph<usize, f64> = DiGraph::new();
    let mut super_idx: HashMap<usize, NodeIndex> = HashMap::new();
    for cid in 0..total_communities {
        let ni = super_g.add_node(cid);
        super_idx.insert(cid, ni);
    }
    let mut edge_sum: HashMap<(usize, usize), f64> = HashMap::new();
    for e in bundle.graph.graph.edge_indices() {
        if let (Some((s, t)), Some(w)) = (
            bundle.graph.graph.edge_endpoints(e),
            bundle.graph.graph.edge_weight(e),
        ) && let (Some(&cs), Some(&ct)) = (comm_for_node.get(&s), comm_for_node.get(&t))
            && cs != ct
        {
            *edge_sum.entry((cs, ct)).or_insert(0.0) += w.weight;
        }
    }
    for ((cs, ct), w) in &edge_sum {
        if let (Some(&ns), Some(&nt)) = (super_idx.get(cs), super_idx.get(ct)) {
            super_g.add_edge(ns, nt, *w);
        }
    }
    // PageRank on the supernode graph using a simpler scheme since weight types differ.
    let n_super = super_g.node_count();
    let inter_pr: HashMap<usize, f64> = if n_super == 0 {
        HashMap::new()
    } else {
        // Adapt: convert super_g into a temporary FileNode/EdgeWeight DiGraph.
        let mut tmp: DiGraph<FileNode, EdgeWeight> = DiGraph::new();
        let mut tmap: HashMap<usize, NodeIndex> = HashMap::new();
        for cid in 0..n_super {
            let ni = tmp.add_node(FileNode {
                file_id: cid as i64,
                relative_path: format!("community_{}", cid),
                language: "community".into(),
                module: "community".into(),
            });
            tmap.insert(cid, ni);
        }
        for ((s, t), w) in &edge_sum {
            if let (Some(&ns), Some(&nt)) = (tmap.get(s), tmap.get(t)) {
                tmp.add_edge(
                    ns,
                    nt,
                    EdgeWeight {
                        edge_type: crate::graph::types::EdgeType::Import,
                        weight: *w,
                    },
                );
            }
        }
        let r = pagerank(&tmp, 0.85, 100, 1e-6);
        r.scores
            .iter()
            .filter_map(|(ni, v)| tmp.node_weight(*ni).map(|n| (n.file_id as usize, *v)))
            .collect()
    };

    let mut rows: Vec<(String, f64, f64, f64, f64)> = Vec::new();
    for ni in bundle.graph.graph.node_indices() {
        let path = bundle
            .graph
            .graph
            .node_weight(ni)
            .map(|n| n.relative_path.clone())
            .unwrap_or_default();
        let cid = comm_for_node.get(&ni).copied().unwrap_or(0);
        let g = global_pr.scores.get(&ni).copied().unwrap_or(0.0);
        let i = intra.get(&ni).copied().unwrap_or(0.0);
        let inter = inter_pr.get(&cid).copied().unwrap_or(0.0);
        let combined = i * (1.0 + inter);
        rows.push((path, g, i, inter, combined));
    }
    rows.sort_by(|a, b| b.4.partial_cmp(&a.4).unwrap_or(std::cmp::Ordering::Equal));
    rows.truncate(limit.max(0) as usize);
    let files: Vec<_> = rows
        .iter()
        .map(|(p, g, intra, inter, comb)| {
            json!({
                "file": p,
                "global_pagerank": g,
                "intra_community": intra,
                "inter_community": inter,
                "combined_score": comb,
            })
        })
        .collect();
    json_result(&json!({
        "project": params.project,
        "communities": total_communities,
        "modularity": louv.modularity,
        "files": files,
        "guidance": "Multi-resolution PageRank distinguishes within-module importance (intra) from module importance (inter). Both leaders and bridges become visible."
    }))
}
