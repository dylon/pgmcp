//! `tool_structural_holes` — Burt 1992 constraint index (SOTA Phase 2.8).

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::graph::algorithms_ext::burt_constraint;
use crate::mcp::server::StructuralHolesParams;
use crate::mcp::tools::fix_helpers::load_import_graph;
use crate::mcp::tools::sota_helpers::{json_result, project_id_or_err};

pub async fn tool_structural_holes(
    ctx: &SystemContext,
    params: StructuralHolesParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "structural_holes", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let bundle = load_import_graph(ctx, project_id).await?;

    let limit = params.limit.unwrap_or(30);
    let sort = params.sort.as_deref().unwrap_or("constraint_asc");

    let scores = burt_constraint(&bundle.graph.graph);
    let mut rows: Vec<(String, f64)> = scores
        .iter()
        .filter_map(|(ni, &c)| {
            bundle
                .graph
                .graph
                .node_weight(*ni)
                .map(|n| (n.relative_path.clone(), c))
        })
        .collect();
    rows.sort_by(|a, b| match sort {
        "constraint_desc" => b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal),
        _ => a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal),
    });
    rows.truncate(limit.max(0) as usize);
    let files: Vec<_> = rows
        .iter()
        .map(|(p, c)| json!({"path": p, "constraint": c}))
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
        "sort": sort,
        "files": files,
        "guidance": "Low Burt's constraint = bridges across structural holes (brokers between otherwise-disconnected neighbourhoods); high constraint = redundantly embedded in a dense cluster. Brokers are high-leverage but single-points-of-failure."
    }))
}
