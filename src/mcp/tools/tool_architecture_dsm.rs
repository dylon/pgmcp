//! `tool_architecture_dsm` — Design Structure Matrix metrics (graph-roadmap
//! Phase 3.2).
//!
//! Propagation cost + core-periphery classification (MacCormack-Rusnak-Baldwin,
//! Management Science 2006) over the file import graph or the function call
//! graph. Propagation cost is the density of the transitive-closure (visibility)
//! matrix — the average fraction of the system reachable from / affected by a
//! random element; the industry maintainability proxy that Lattix / Structure101
//! report. Files are classified Core / Shared / Control / Peripheral by their
//! visibility fan-in (VFI) and fan-out (VFO), and the largest cyclic group
//! (largest SCC) is surfaced as the architectural core.

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::graph::dsm::{CorePeriphery, analyze_dsm, classify_core_periphery};
use crate::mcp::server::ArchitectureDsmParams;
use crate::mcp::tools::graph_scope::load_scoped_graph;
use crate::mcp::tools::sota_helpers::{json_result, project_id_or_err};

pub async fn tool_architecture_dsm(
    ctx: &SystemContext,
    params: ArchitectureDsmParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "architecture_dsm", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let scope = params.scope.as_deref().unwrap_or("file");
    let limit = params.limit.unwrap_or(20).clamp(1, 500) as usize;

    let g = load_scoped_graph(ctx, project_id, scope).await?;
    let n = g.node_count();
    if n == 0 {
        return json_result(&json!({
            "project": params.project,
            "scope": scope,
            "node_count": 0,
            "guidance": "Empty dependency graph — run symbol-extraction / graph-analysis crons first."
        }));
    }

    let dsm = analyze_dsm(&g);
    let cp = classify_core_periphery(&g, &dsm);

    // Class counts.
    let mut counts = [0usize; 4]; // core, shared, control, peripheral
    for c in &cp.classes {
        let i = match c {
            CorePeriphery::Core => 0,
            CorePeriphery::Shared => 1,
            CorePeriphery::Control => 2,
            CorePeriphery::Peripheral => 3,
        };
        counts[i] += 1;
    }

    // Per-node rows enriched with visibility metrics + class, for ranking.
    let mut rows: Vec<(usize, usize, usize, CorePeriphery, serde_json::Value)> = (0..n)
        .filter_map(|i| {
            let ni = petgraph::graph::NodeIndex::new(i);
            g.node_weight(ni).map(|m| {
                (
                    dsm.visibility_fan_in[i],
                    dsm.visibility_fan_out[i],
                    i,
                    cp.classes[i],
                    m.to_json(),
                )
            })
        })
        .collect();

    // Top by VFI (most depended-upon — change-risk hubs).
    rows.sort_by(|a, b| b.0.cmp(&a.0).then(a.2.cmp(&b.2)));
    let top_fan_in: Vec<_> = rows
        .iter()
        .take(limit)
        .map(|(vfi, vfo, _, class, node)| {
            json!({ "node": node, "class": class.as_str(),
                    "visibility_fan_in": vfi, "visibility_fan_out": vfo })
        })
        .collect();

    // Top by VFO (widest blast radius — a change here reaches the most code).
    rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.2.cmp(&b.2)));
    let top_fan_out: Vec<_> = rows
        .iter()
        .take(limit)
        .map(|(vfi, vfo, _, class, node)| {
            json!({ "node": node, "class": class.as_str(),
                    "visibility_fan_in": vfi, "visibility_fan_out": vfo })
        })
        .collect();

    // Cyclic-core members (resolve indices → node labels).
    let cyclic_core: Vec<_> = cp
        .cyclic_core
        .iter()
        .take(limit)
        .filter_map(|&i| {
            g.node_weight(petgraph::graph::NodeIndex::new(i))
                .map(|m| m.to_json())
        })
        .collect();

    let pc = dsm.propagation_cost;
    let pc_band = if pc < 0.05 {
        "low (loosely coupled / well-decoupled)"
    } else if pc < 0.20 {
        "moderate"
    } else {
        "high (a change ripples across much of the system)"
    };

    json_result(&json!({
        "project": params.project,
        "scope": scope,
        "node_count": n,
        "propagation_cost": format!("{:.4}", pc),
        "propagation_cost_band": pc_band,
        "vfi_threshold": format!("{:.1}", cp.vfi_threshold),
        "vfo_threshold": format!("{:.1}", cp.vfo_threshold),
        "class_counts": {
            "core": counts[0],
            "shared": counts[1],
            "control": counts[2],
            "peripheral": counts[3],
        },
        "cyclic_core_size": cp.cyclic_core.len(),
        "cyclic_core": cyclic_core,
        "top_by_visibility_fan_in": top_fan_in,
        "top_by_visibility_fan_out": top_fan_out,
        "guidance": "Propagation cost is the density of the transitive-closure (visibility) matrix — \
            the average fraction of the system reachable from a random file; lower is more loosely \
            coupled. The MacCormack quadrants split files by visibility fan-in (VFI = how much of the \
            system can reach this file) and fan-out (VFO = how much this file can reach), at the median: \
            Core (hub, high both), Shared (utility, high VFI), Control (orchestrator, high VFO), \
            Peripheral (leaf). A large `cyclic_core` (the dominant SCC) is the classic 'hidden structure' \
            risk — break it with `circular_dependencies` / `recommend_module_split`. High-VFO files are \
            the widest blast radius for a change; high-VFI files concentrate change risk."
    }))
}
