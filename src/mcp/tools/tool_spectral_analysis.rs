//! `tool_spectral_analysis` — spectral connectivity + WL structural clones
//! (graph-roadmap Phase 4.6) over the file import graph or function call graph.
//!
//! - **Algebraic connectivity** λ₂ + **Fiedler bipartition** (Fiedler 1973;
//!   Shi-Malik normalized cut): global robustness + a natural balanced module
//!   boundary (the two sign-sides of the Fiedler vector).
//! - **WL structural clones** (Weisfeiler-Lehman, JMLR 2011): nodes with
//!   identical refined neighborhood structure — call-graph shapes that repeat
//!   despite renamed identifiers, complementing the text-level
//!   `lsh_clone_detection`.

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::graph::spectral::algebraic_connectivity;
use crate::graph::wl_hash::structural_clone_classes;
use crate::mcp::server::SpectralAnalysisParams;
use crate::mcp::tools::graph_scope::load_scoped_graph;
use crate::mcp::tools::sota_helpers::{json_result, project_id_or_err};

/// Spectral power iteration is O(iters·n); cap the Fiedler computation here.
const MAX_SPECTRAL_NODES: usize = 5000;

pub async fn tool_spectral_analysis(
    ctx: &SystemContext,
    params: SpectralAnalysisParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "spectral_analysis", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let scope = params.scope.as_deref().unwrap_or("file");
    let limit = params.limit.unwrap_or(20).clamp(1, 500) as usize;
    let wl_iterations = params.wl_iterations.unwrap_or(2).clamp(1, 6) as usize;

    let g = load_scoped_graph(ctx, project_id, scope).await?;
    let n = g.node_count();
    if n == 0 {
        return json_result(&json!({
            "project": params.project,
            "scope": scope,
            "node_count": 0,
            "guidance": "Empty graph — run symbol-extraction / graph-analysis crons first."
        }));
    }

    // Spectral bisection (gated on node count).
    let spectral = if n <= MAX_SPECTRAL_NODES {
        algebraic_connectivity(&g).map(|s| {
            let (mut side_a, mut side_b) = (Vec::new(), Vec::new());
            for (i, &val) in s.fiedler.iter().enumerate() {
                let label = g
                    .node_weight(petgraph::graph::NodeIndex::new(i))
                    .map(|m| m.label.clone())
                    .unwrap_or_default();
                if val >= 0.0 {
                    side_a.push(label);
                } else {
                    side_b.push(label);
                }
            }
            let band = if s.algebraic_connectivity < 1e-6 {
                "disconnected (λ₂≈0)"
            } else if s.algebraic_connectivity < 0.1 {
                "near-bottleneck (weak global seam)"
            } else {
                "robustly connected"
            };
            json!({
                "algebraic_connectivity": format!("{:.4}", s.algebraic_connectivity),
                "band": band,
                "converged": s.converged,
                "bisection_size_a": side_a.len(),
                "bisection_size_b": side_b.len(),
                "bisection_a_sample": side_a.into_iter().take(limit).collect::<Vec<_>>(),
                "bisection_b_sample": side_b.into_iter().take(limit).collect::<Vec<_>>(),
            })
        })
    } else {
        None
    };

    // WL structural clones.
    let classes = structural_clone_classes(&g, wl_iterations);
    let clone_json: Vec<serde_json::Value> = classes
        .iter()
        .take(limit)
        .map(|c| {
            let members: Vec<String> = c
                .iter()
                .take(12)
                .filter_map(|&ni| g.node_weight(ni).map(|m| m.label.clone()))
                .collect();
            json!({ "size": c.len(), "members": members })
        })
        .collect();

    json_result(&json!({
        "project": params.project,
        "scope": scope,
        "node_count": n,
        "spectral": spectral,
        "spectral_skipped_large_graph": n > MAX_SPECTRAL_NODES,
        "wl_iterations": wl_iterations,
        "structural_clone_classes": clone_json,
        "structural_clone_class_count": classes.len(),
        "guidance": "algebraic_connectivity (λ₂) gauges global robustness: ≈0 means the graph is \
            disconnected or near-bottlenecked; the Fiedler bisection (sign sides) is a natural balanced \
            module boundary to consider for a split. structural_clone_classes group nodes whose \
            Weisfeiler-Lehman neighborhood structure is identical — repeated call/import shapes even after \
            renaming (extract a shared abstraction). Complements text-level `lsh_clone_detection` and the \
            min-cut in `graph_connectivity`."
    }))
}
