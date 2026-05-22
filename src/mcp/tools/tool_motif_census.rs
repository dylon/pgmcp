//! `tool_motif_census` — 3-node + 4-node graphlet census (SOTA Phase 2.9,
//! Milo et al. Science 2002; Pržulj GDD 2007).

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::graph::algorithms_ext::motif_census;
use crate::mcp::server::MotifCensusParams;
use crate::mcp::tools::fix_helpers::load_import_graph;
use crate::mcp::tools::sota_helpers::{json_result, project_id_or_err};

const TRIAD_NAMES: &[&str] = &[
    "003",
    "012",
    "102",
    "021D",
    "021U",
    "021C",
    "111",
    "030T-or-other",
    "030T",
    "030C",
    "201",
    "120-family",
    "120D-or-U-or-C",
    "120C",
    "210",
    "300",
];

pub async fn tool_motif_census(
    ctx: &SystemContext,
    params: MotifCensusParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "motif_census", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let bundle = load_import_graph(ctx, project_id).await?;

    let result = motif_census(&bundle.graph.graph);

    let triads: Vec<_> = TRIAD_NAMES
        .iter()
        .zip(result.triads.iter())
        .map(|(name, count)| json!({"motif": name, "count": count}))
        .collect();
    json_result(&json!({
        "project": params.project,
        "n_nodes": bundle.graph.graph.node_count(),
        "n_edges": bundle.graph.graph.edge_count(),
        "triads": triads,
        "graphlets_4": {
            "cliques": result.graphlets_4[0],
            "directed_stars": result.graphlets_4[1],
        },
        "guidance": "Triad census follows the Davis-Leinhardt taxonomy. Architecture signature: high 030T = transitive (clean layering); high 030C = circular dependencies; high cliques = god-clusters."
    }))
}
