//! `tool_kcore_analysis` — K-core decomposition (SOTA Phase 2.1, Seidman 1983).

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::graph::algorithms_ext::k_core_decomposition;
use crate::mcp::server::KcoreAnalysisParams;
use crate::mcp::tools::fix_helpers::load_import_graph;
use crate::mcp::tools::sota_helpers::{json_result, project_id_or_err, text_result};

pub async fn tool_kcore_analysis(
    ctx: &SystemContext,
    params: KcoreAnalysisParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "kcore_analysis", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let bundle = load_import_graph(ctx, project_id).await?;
    let result = k_core_decomposition(&bundle.graph.graph);
    let min_core = params.min_core.unwrap_or(0);
    let limit = params.limit.unwrap_or(50);

    let mut rows: Vec<(String, u32)> = result
        .coreness
        .iter()
        .filter_map(|(ni, &c)| {
            if c < min_core {
                return None;
            }
            bundle
                .graph
                .graph
                .node_weight(*ni)
                .map(|n| (n.relative_path.clone(), c))
        })
        .collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rows.truncate(limit.max(0) as usize);

    let files: Vec<_> = rows
        .iter()
        .map(|(p, c)| json!({"path": p, "coreness": c}))
        .collect();
    json_result(&json!({
        "project": params.project,
        "max_core": result.max_core,
        "min_core_filter": min_core,
        "files": files,
        "guidance": "Coreness = highest k such that the file belongs to a subgraph where every node has at least k undirected neighbours. High coreness = load-bearing structural backbone."
    }))
}
