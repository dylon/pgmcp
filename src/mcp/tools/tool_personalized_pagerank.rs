//! `tool_personalized_pagerank` — Personalized PageRank with restart
//! (SOTA Phase 2.3, Tong-Faloutsos-Pan ICDM 2006).

#![allow(unused_imports)]

use std::collections::HashMap;
use std::sync::atomic::Ordering;

use petgraph::graph::NodeIndex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::graph::algorithms_ext::personalized_pagerank;
use crate::mcp::server::PersonalizedPagerankParams;
use crate::mcp::tools::fix_helpers::load_import_graph;
use crate::mcp::tools::sota_helpers::{json_result, project_id_or_err};

pub async fn tool_personalized_pagerank(
    ctx: &SystemContext,
    params: PersonalizedPagerankParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "personalized_pagerank", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let bundle = load_import_graph(ctx, project_id).await?;

    let damping = params.damping.unwrap_or(0.85);
    let limit = params.limit.unwrap_or(50);

    // Resolve seed file paths to NodeIndex via the graph.
    let mut seeds: HashMap<NodeIndex, f64> = HashMap::new();
    for path in &params.seed_files {
        for ni in bundle.graph.graph.node_indices() {
            if let Some(node) = bundle.graph.graph.node_weight(ni)
                && node.relative_path == *path
            {
                seeds.insert(ni, 1.0);
            }
        }
    }
    if seeds.is_empty() {
        return json_result(&json!({
            "project": params.project,
            "error": "No seed files matched any indexed paths",
            "seed_files": params.seed_files,
        }));
    }

    let result = personalized_pagerank(&bundle.graph.graph, &seeds, damping, 100, 1e-6);

    let mut rows: Vec<(String, f64)> = result
        .scores
        .iter()
        .filter_map(|(ni, &s)| {
            bundle
                .graph
                .graph
                .node_weight(*ni)
                .map(|n| (n.relative_path.clone(), s))
        })
        .collect();
    rows.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    rows.truncate(limit.max(0) as usize);
    let files: Vec<_> = rows
        .iter()
        .map(|(p, s)| json!({"path": p, "ppr_score": s}))
        .collect();
    json_result(&json!({
        "project": params.project,
        "seeds": params.seed_files,
        "damping": damping,
        "converged": result.converged,
        "iterations": result.iterations,
        "files": files,
        "guidance": "Personalized PageRank rates every file by its proximity to the seed set under random-walk-with-restart. Use for blast-radius / impact analysis from a known change."
    }))
}
