//! `tool_extended_centrality` — the centrality measures that were implemented
//! in `algorithms_ext.rs` but exposed by no tool (eigenvector, Katz, harmonic),
//! plus two newly added ones (closeness, reverse-PageRank), surfaced over
//! either the file import graph or the function call graph (graph-roadmap
//! Phase 1.2). Computed in real time on the requested graph — these aren't
//! materialized, but they're cheap to recompute per query.

use std::collections::HashMap;

use petgraph::graph::{DiGraph, NodeIndex};
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::db::queries;
use crate::graph::algorithms_ext::{
    closeness_centrality, eigenvector_centrality, harmonic_centrality, katz_centrality,
    reverse_pagerank,
};
use crate::graph::call_graph::{CallGraph, FunctionNode, RawCallEdge};
use crate::graph::types::EdgeCost;
use crate::mcp::server::ExtendedCentralityParams;
use crate::mcp::tools::fix_helpers::load_import_graph;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

const VALID_METRICS: [&str; 5] = [
    "eigenvector",
    "katz",
    "harmonic",
    "closeness",
    "reverse_pagerank",
];

/// Dispatch to the requested centrality over any `DiGraph<N, E: EdgeCost>`.
/// `metric` is validated by the caller; an unknown value yields an empty map.
fn compute_metric<N, E: EdgeCost>(
    graph: &DiGraph<N, E>,
    metric: &str,
    alpha: f64,
    beta: f64,
) -> HashMap<NodeIndex, f64> {
    match metric {
        "eigenvector" => eigenvector_centrality(graph, 100, 1e-6),
        "katz" => katz_centrality(graph, alpha, beta, 100, 1e-6),
        "harmonic" => harmonic_centrality(graph),
        "closeness" => closeness_centrality(graph),
        "reverse_pagerank" => reverse_pagerank(graph, 0.85, 100, 1e-6),
        _ => HashMap::new(),
    }
}

pub async fn tool_extended_centrality(
    ctx: &SystemContext,
    params: ExtendedCentralityParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "extended_centrality", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;

    let metric = params.metric.as_deref().unwrap_or("eigenvector");
    if !VALID_METRICS.contains(&metric) {
        return Err(McpError::invalid_params(
            format!(
                "Unknown metric '{}'. Use one of: {}.",
                metric,
                VALID_METRICS.join(", ")
            ),
            None,
        ));
    }
    let scope = params.scope.as_deref().unwrap_or("file");
    let limit = params.limit.unwrap_or(50).clamp(1, 1000) as usize;
    let alpha = params.alpha.unwrap_or(0.1);
    let beta = params.beta.unwrap_or(1.0);

    // (label, optional file path, score) — for file scope, label IS the path
    // and the file field is None; for function scope, label is the function.
    let mut entries: Vec<(String, Option<String>, f64)> = match scope {
        "function" => {
            let node_rows = queries::list_function_nodes_for_project(pool, project_id)
                .await
                .map_err(|e| {
                    McpError::internal_error(format!("function nodes query failed: {}", e), None)
                })?;
            let raws = queries::list_call_edges_for_project(pool, project_id)
                .await
                .map_err(|e| {
                    McpError::internal_error(format!("call edges query failed: {}", e), None)
                })?;
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
            let cg = CallGraph::build(nodes, edges);
            let scores = compute_metric(&cg.graph, metric, alpha, beta);
            scores
                .iter()
                .filter_map(|(ni, &s)| {
                    cg.graph
                        .node_weight(*ni)
                        .map(|f| (f.name.clone(), Some(f.relative_path.clone()), s))
                })
                .collect()
        }
        "file" => {
            let bundle = load_import_graph(ctx, project_id).await?;
            let scores = compute_metric(&bundle.graph.graph, metric, alpha, beta);
            scores
                .iter()
                .filter_map(|(ni, &s)| {
                    bundle
                        .graph
                        .graph
                        .node_weight(*ni)
                        .map(|n| (n.relative_path.clone(), None, s))
                })
                .collect()
        }
        other => {
            return Err(McpError::invalid_params(
                format!(
                    "Unknown scope '{}'. Use \"file\" (default) or \"function\".",
                    other
                ),
                None,
            ));
        }
    };

    entries.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    entries.truncate(limit);

    let results: Vec<_> = entries
        .into_iter()
        .map(|(label, file, score)| match file {
            Some(path) => json!({ "function": label, "file": path, "score": score }),
            None => json!({ "file": label, "score": score }),
        })
        .collect();

    json_result(&json!({
        "project": params.project,
        "metric": metric,
        "scope": scope,
        "results": results,
        "guidance": match metric {
            "eigenvector" => "Eigenvector centrality (Bonacich): importance proportional to the importance of neighbours — finds nodes embedded among other important nodes, not just high-degree ones.",
            "katz" => "Katz centrality: eigenvector-style influence with an attenuated walk sum plus a base term, robust on directed/acyclic graphs where plain eigenvector degenerates. Tune `alpha` below 1/λ_max.",
            "harmonic" => "Harmonic centrality (Marchiori-Latora): mean inverse distance — a reach measure well-defined even on disconnected graphs.",
            "closeness" => "Closeness centrality (Wasserman-Faust normalized): nodes that reach the rest of the system in the fewest hops — best vantage points / smallest blast radius.",
            "reverse_pagerank" => "Reverse PageRank / SinkRank: foundational sinks that much of the system ultimately depends on — the dual of PageRank's depends-on-everything hubs.",
            _ => "",
        }
    }))
}
