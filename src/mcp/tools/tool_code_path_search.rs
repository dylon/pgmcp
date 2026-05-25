//! `tool_code_path_search` — PathRAG over the code graph (Chen et al. 2025),
//! ported from the memory server's `memory_path_search`. (graph-roadmap Phase 3.3)
//!
//! Where `code_ppr_search` ranks *files* by relational proximity, PathRAG
//! returns the actual *routes*: the import / call / co_change / semantic chains
//! that connect the query's dense-similar files to related code. Flow-pruned
//! (PathRAG reliability flow) and hop-capped so the walk stays bounded. Answers
//! "how does A reach B / what chain links these" in one shot.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::Instant;

use petgraph::graph::NodeIndex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::db::queries;
use crate::graph::pathrank::ranked_paths;
use crate::mcp::server::CodePathSearchParams;
use crate::mcp::tools::fix_helpers::load_code_graph_all_edges;
use crate::mcp::tools::sota_helpers::{json_result, project_id_or_err};

pub async fn tool_code_path_search(
    ctx: &SystemContext,
    params: CodePathSearchParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "code_path_search", "MCP tool invoked");
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);

    let project_id = project_id_or_err(ctx, &params.project).await?;
    let max_hops = params.max_hops.unwrap_or(4).clamp(1, 6) as usize;
    let max_seeds = params.max_seeds.unwrap_or(5).clamp(1, 50);
    let k = params.k.unwrap_or(15).clamp(1, 200) as usize;
    let min_flow = params.min_flow.unwrap_or(0.1).clamp(0.0, 1.0);

    let pool = ctx
        .db()
        .pool()
        .ok_or_else(|| McpError::internal_error("no database pool", None))?;
    let ef = ctx.config().load().vector.ef_search;

    let embedding = ctx
        .embed()
        .embed_query(&params.query)
        .await
        .map_err(|e| McpError::internal_error(format!("embed failed: {}", e), None))?;

    let seed_files = queries::ppr_seed_files(pool, &embedding, project_id, max_seeds, ef)
        .await
        .map_err(|e| McpError::internal_error(format!("seed query failed: {}", e), None))?;
    if seed_files.is_empty() {
        return json_result(&json!({
            "project": params.project,
            "paths": [],
            "guidance": "No chunks matched the query in this project. Try `semantic_search`."
        }));
    }

    let bundle = load_code_graph_all_edges(ctx, project_id).await?;
    let cg = &bundle.graph;

    let seeds: Vec<(NodeIndex, f64)> = seed_files
        .iter()
        .filter_map(|(fid, sim)| cg.file_id_to_node.get(fid).map(|&ni| (ni, sim.max(0.0))))
        .collect();
    if seeds.is_empty() {
        return json_result(&json!({
            "project": params.project,
            "paths": [],
            "seed_files": seed_files.len(),
            "guidance": "Seed files have no graph edges yet — run the symbol-extraction / call-graph / \
                         graph-analysis / semantic-edges crons, then retry. (`code_ppr_search` falls \
                         back to dense order; this tool needs edges to trace paths.)"
        }));
    }

    let paths = ranked_paths(&cg.graph, &seeds, max_hops, min_flow, k);

    let path_json: Vec<serde_json::Value> = paths
        .iter()
        .map(|p| {
            // Resolve node indices → file paths, and each hop → its edge type.
            let files: Vec<&str> = p
                .nodes
                .iter()
                .filter_map(|&ni| cg.graph.node_weight(ni).map(|f| f.relative_path.as_str()))
                .collect();
            let hops: Vec<serde_json::Value> = p
                .nodes
                .windows(2)
                .zip(p.edge_weights.iter())
                .map(|(w, weight)| {
                    let etype = cg
                        .graph
                        .find_edge(w[0], w[1])
                        .and_then(|e| cg.graph.edge_weight(e))
                        .map(|ew| ew.edge_type.as_str())
                        .unwrap_or("?");
                    json!({ "edge_type": etype, "weight": format!("{:.3}", weight) })
                })
                .collect();
            json!({
                "files": files,
                "hops": hops,
                "length": p.edge_weights.len(),
                "flow": format!("{:.4}", p.flow),
            })
        })
        .collect();

    json_result(&json!({
        "project": params.project,
        "seed_files": seed_files.len(),
        "graph_nodes": cg.graph.node_count(),
        "graph_edges": cg.graph.edge_count(),
        "max_hops": max_hops,
        "min_flow": min_flow,
        "path_count": path_json.len(),
        "paths": path_json,
        "elapsed_ms": start.elapsed().as_millis() as u64,
        "guidance": "PathRAG: ranked, flow-pruned dependency routes from the query's dense-similar files \
            through import/call/co_change/semantic edges. `flow` is the product of edge weights along the \
            path (higher = stronger, shorter routes win ties); each hop is labeled with its edge_type. Use \
            to trace 'how does A reach B' or the strongest chain linking a query hit to related code. \
            Complements `code_ppr_search` (which ranks files, not routes)."
    }))
}
