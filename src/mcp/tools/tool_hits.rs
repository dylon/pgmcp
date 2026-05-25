//! `tool_hits` — Kleinberg HITS hubs & authorities (graph-roadmap Phase 2.6).
//!
//! Over the file import graph or the function call graph: separates
//! *orchestrators* (hubs — point to many good authorities) from *utilities*
//! (authorities — pointed to by many good hubs), a distinction PageRank
//! conflates into a single score.

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::graph::algorithms_ext::hits;
use crate::mcp::server::HitsParams;
use crate::mcp::tools::graph_scope::load_scoped_graph;
use crate::mcp::tools::sota_helpers::{json_result, project_id_or_err};

pub async fn tool_hits(
    ctx: &SystemContext,
    params: HitsParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "hits", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let scope = params.scope.as_deref().unwrap_or("file");
    let limit = params.limit.unwrap_or(25).clamp(1, 500) as usize;

    let g = load_scoped_graph(ctx, project_id, scope).await?;
    let r = hits(&g, 100, 1e-8);

    let top = |scores: &std::collections::HashMap<petgraph::graph::NodeIndex, f64>| {
        let mut v: Vec<_> = scores.iter().map(|(&ni, &s)| (ni, s)).collect();
        v.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.index().cmp(&b.0.index()))
        });
        v.into_iter()
            .take(limit)
            .filter_map(|(ni, s)| {
                g.node_weight(ni).map(|m| {
                    let mut obj = m.to_json();
                    if let Some(map) = obj.as_object_mut() {
                        map.insert("score".into(), json!(s));
                    }
                    obj
                })
            })
            .collect::<Vec<_>>()
    };

    let hubs = top(&r.hubs);
    let authorities = top(&r.authorities);

    json_result(&json!({
        "project": params.project,
        "scope": scope,
        "hubs": hubs,
        "authorities": authorities,
        "guidance": "HITS separates two roles PageRank blends: hubs point to many important nodes \
            (orchestrators / entry points / composition roots), authorities are pointed to by many \
            important nodes (core utilities / foundational modules). On a call graph, top hubs are the \
            functions that drive the most machinery; top authorities are the load-bearing leaves \
            everything funnels into. Read top authorities first to learn the core; treat top hubs as the \
            risky change-amplifiers."
    }))
}
