//! `tool_ktruss_analysis` — K-truss decomposition (SOTA Phase 2.2, Cohen 2008).

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::graph::algorithms_ext::k_truss_decomposition;
use crate::mcp::server::KtrussAnalysisParams;
use crate::mcp::tools::fix_helpers::load_import_graph;
use crate::mcp::tools::sota_helpers::{json_result, project_id_or_err};

pub async fn tool_ktruss_analysis(
    ctx: &SystemContext,
    params: KtrussAnalysisParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "ktruss_analysis", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let bundle = load_import_graph(ctx, project_id).await?;
    let result = k_truss_decomposition(&bundle.graph.graph);

    let limit = params.limit.unwrap_or(50);
    let min_truss = params.min_truss.unwrap_or(3);

    let mut rows: Vec<(String, String, u32)> = result
        .edge_trussness
        .iter()
        .filter_map(|(&(a, b), &t)| {
            if t < min_truss {
                return None;
            }
            let an = bundle.graph.graph.node_weight(a)?;
            let bn = bundle.graph.graph.node_weight(b)?;
            Some((an.relative_path.clone(), bn.relative_path.clone(), t))
        })
        .collect();
    rows.sort_by(|x, y| {
        y.2.cmp(&x.2)
            .then_with(|| x.0.cmp(&y.0))
            .then_with(|| x.1.cmp(&y.1))
    });
    rows.truncate(limit.max(0) as usize);

    let edges: Vec<_> = rows
        .iter()
        .map(|(s, t, k)| json!({"source": s, "target": t, "trussness": k}))
        .collect();
    json_result(&json!({
        "project": params.project,
        "max_truss": result.max_truss,
        "min_truss_filter": min_truss,
        "edges": edges,
        "guidance": "Trussness k = highest k such that every edge sits in at least k−2 triangles. High trussness = dense cohesive module; low trussness on a critical edge = fragile link."
    }))
}
