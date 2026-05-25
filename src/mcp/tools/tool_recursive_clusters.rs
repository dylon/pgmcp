//! `tool_recursive_clusters` — mutual- and direct-recursion in the call graph
//! (graph-roadmap Phase 1.1).
//!
//! Builds the symbol-resolved call graph in real time and reports its
//! strongly-connected components of size ≥ 2 (mutual recursion) together with
//! the concrete call cycles inside each, plus functions that call themselves
//! (direct recursion). `CallGraph::sccs()` already existed but was unexposed;
//! this surfaces it and pairs it with the now-generic `extract_simple_cycles`.

use std::collections::{HashMap, HashSet};

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::db::queries;
use crate::graph::call_graph::{CallGraph, FunctionNode, RawCallEdge};
use crate::mcp::server::RecursiveClustersParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_recursive_clusters(
    ctx: &SystemContext,
    params: RecursiveClustersParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "recursive_clusters", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;

    let max_cycle_len = params.max_cycle_len.unwrap_or(8).clamp(2, 32) as usize;
    let limit = params.limit.unwrap_or(50).clamp(1, 1000) as usize;

    let node_rows = queries::list_function_nodes_for_project(pool, project_id)
        .await
        .map_err(|e| {
            McpError::internal_error(format!("function nodes query failed: {}", e), None)
        })?;
    let raws = queries::list_call_edges_for_project(pool, project_id)
        .await
        .map_err(|e| McpError::internal_error(format!("call edges query failed: {}", e), None))?;

    // symbol_id -> (function name, file) for labeling direct-recursion hits.
    let mut label: HashMap<i64, (String, String)> = HashMap::with_capacity(node_rows.len());
    for n in &node_rows {
        label.insert(n.symbol_id, (n.name.clone(), n.relative_path.clone()));
    }

    // Direct recursion: a resolved call edge whose source == target symbol.
    let mut direct: Vec<serde_json::Value> = Vec::new();
    let mut seen_self: HashSet<i64> = HashSet::new();
    for r in &raws {
        if let (Some(s), Some(t)) = (r.source_symbol_id, r.target_symbol_id)
            && s == t
            && seen_self.insert(s)
            && let Some((name, file)) = label.get(&s)
        {
            direct.push(json!({ "symbol_id": s, "function": name, "file": file }));
        }
    }

    let nodes: Vec<FunctionNode> = node_rows
        .into_iter()
        .map(|n| FunctionNode {
            symbol_id: n.symbol_id,
            file_id: n.file_id,
            name: n.name,
            relative_path: n.relative_path,
            language: n.language,
            is_method: n.parent_id.is_some(),
        })
        .collect();

    let edges: Vec<RawCallEdge> = raws
        .iter()
        .filter_map(|r| {
            r.source_symbol_id.map(|src| RawCallEdge {
                source_symbol_id: src,
                target_symbol_id: r.target_symbol_id,
                target_raw: r.target_raw.clone(),
                weight: 1.0,
            })
        })
        .collect();

    let graph = CallGraph::build(nodes, edges);

    // SCCs of size ≥ 2 are mutual-recursion clusters. Extract the concrete
    // simple cycles within each (as function-name sequences).
    let mut clusters: Vec<serde_json::Value> = Vec::new();
    for scc in graph.sccs().into_iter().filter(|c| c.len() >= 2) {
        let members: Vec<_> = scc
            .iter()
            .filter_map(|&idx| {
                graph.graph.node_weight(idx).map(|f| {
                    json!({
                        "symbol_id": f.symbol_id, "function": f.name, "file": f.relative_path,
                    })
                })
            })
            .collect();
        let cycles: Vec<Vec<String>> =
            crate::graph::algorithms::extract_simple_cycles(&graph.graph, &scc, max_cycle_len)
                .into_iter()
                .take(20)
                .map(|cyc| {
                    cyc.iter()
                        .filter_map(|&idx| graph.graph.node_weight(idx).map(|f| f.name.clone()))
                        .collect()
                })
                .collect();
        clusters.push(json!({
            "size": members.len(),
            "members": members,
            "cycles": cycles,
        }));
    }
    clusters.sort_by(|a, b| {
        b["size"]
            .as_u64()
            .unwrap_or(0)
            .cmp(&a["size"].as_u64().unwrap_or(0))
    });
    let total_clusters = clusters.len();
    clusters.truncate(limit);

    json_result(&json!({
        "project": params.project,
        "nodes": graph.node_count(),
        "resolved_edges": graph.edge_count(),
        "total_mutual_recursion_clusters": total_clusters,
        "mutual_recursion_clusters": clusters,
        "direct_recursion": direct,
        "guidance": "Mutual-recursion clusters are strongly-connected components (size ≥ 2) in the call \
            graph — functions that transitively call one another; each `cycles` entry is a concrete call \
            loop. `direct_recursion` lists self-calling functions. Cycles are intentional for some \
            algorithms but are a refactoring smell when accidental: they block layering, complicate \
            isolated testing, and risk unbounded recursion. NOTE: edges come from name-based call \
            resolution, so a cluster can include spurious edges from same-named methods on unrelated \
            types — confirm before acting (type-aware resolution lands in a later phase)."
    }))
}
