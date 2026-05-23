//! `tool_edge_betweenness` — Brandes edge variant (SOTA Phase 2.4, Girvan-Newman 2002).

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::graph::algorithms_ext::edge_betweenness;
use crate::mcp::server::EdgeBetweennessParams;
use crate::mcp::tools::fix_helpers::load_import_graph;
use crate::mcp::tools::sota_helpers::{json_result, project_id_or_err};

pub async fn tool_edge_betweenness(
    ctx: &SystemContext,
    params: EdgeBetweennessParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "edge_betweenness", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let bundle = load_import_graph(ctx, project_id).await?;

    let limit = params.limit.unwrap_or(50);

    let scores = edge_betweenness(&bundle.graph.graph);
    let mut rows: Vec<(String, String, f64)> = scores
        .iter()
        .filter_map(|(&(a, b), &v)| {
            let an = bundle.graph.graph.node_weight(a)?;
            let bn = bundle.graph.graph.node_weight(b)?;
            Some((an.relative_path.clone(), bn.relative_path.clone(), v))
        })
        .collect();
    rows.sort_by(|x, y| y.2.partial_cmp(&x.2).unwrap_or(std::cmp::Ordering::Equal));
    rows.truncate(limit.max(0) as usize);
    let edges: Vec<_> = rows
        .iter()
        .map(|(s, t, v)| json!({"source": s, "target": t, "betweenness": v}))
        .collect();
    // Shadow-ASR channel (Phase D2b): per-effect symbol-count breakdown
    // for the project. Universal enrichment — every tool benefits from
    // surfacing the effect distribution alongside its primary output.
    // Gracefully degrades to empty when the project lookup or
    // shadow-ASR data isn't populated.
    let effect_breakdown: Vec<serde_json::Value> = (async {
        let Some(pool) = ctx.db().pool() else {
            return Vec::new();
        };
        let project_id_opt: Option<i32> =
            sqlx::query_scalar("SELECT id FROM projects WHERE name = $1")
                .bind(&params.project)
                .fetch_optional(pool)
                .await
                .unwrap_or(None);
        match project_id_opt {
            Some(pid) => crate::mcp::tools::sema_helpers::effects::effect_counts(pool, pid)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
                .collect(),
            None => Vec::new(),
        }
    })
    .await;

    json_result(&json!({
        "effect_breakdown": effect_breakdown,
        "project": params.project,
        "edges": edges,
        "guidance": "Edge betweenness counts how many shortest paths route across each edge. Highest-rank edges are bottlenecks — removing them disconnects the graph or doubles path length."
    }))
}
