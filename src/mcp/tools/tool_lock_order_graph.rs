//! `tool_lock_order_graph` — inspect the interprocedural lock-order graph
//! (nodes = lock resources, edges = "B acquired while A held", plus the cyclic
//! SCCs). Companion to `deadlock_cycles` for drilling into why a cycle exists.

use std::collections::BTreeSet;
use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::concurrency::{self, LockOrderOptions};
use crate::context::SystemContext;
use crate::graph::lock_order::{self, AcqMode};
use crate::mcp::server::LockOrderGraphParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

fn mode_str(m: AcqMode) -> &'static str {
    match m {
        AcqMode::Read => "read",
        AcqMode::Write => "write",
    }
}

fn normalize_confidence_floor(raw: Option<f32>) -> Result<f32, McpError> {
    let value = raw.unwrap_or(0.3);
    if !value.is_finite() {
        return Err(McpError::invalid_params(
            "confidence_floor must be finite",
            None,
        ));
    }
    Ok(value.clamp(0.0, 1.0))
}

pub async fn tool_lock_order_graph(
    ctx: &SystemContext,
    params: LockOrderGraphParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "lock_order_graph", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project = params.project.trim();
    let project_id = project_id_or_err(ctx, project).await?;
    let pool = pool_or_err(ctx)?;

    let opts = LockOrderOptions {
        max_call_depth: params.max_call_depth.unwrap_or(5).clamp(1, 12),
        confidence_floor: normalize_confidence_floor(params.confidence_floor)?,
        max_cycle_len: 6,
        call_confidence: 0.5,
    };

    let edges = concurrency::lock_order_edges(pool, project_id, opts)
        .await
        .map_err(|e| McpError::internal_error(format!("lock-order graph failed: {e}"), None))?;

    let focus = params.resource_key.as_deref();
    let view: Vec<&lock_order::LockEdge> = edges
        .iter()
        .filter(|e| focus.is_none_or(|k| e.from == k || e.to == k))
        .collect();

    let mut nodes: BTreeSet<&str> = BTreeSet::new();
    for e in &view {
        nodes.insert(e.from.as_str());
        nodes.insert(e.to.as_str());
    }
    let edges_json: Vec<_> = view
        .iter()
        .map(|e| {
            json!({
                "from": e.from, "to": e.to,
                "from_mode": mode_str(e.from_mode), "to_mode": mode_str(e.to_mode),
                "interprocedural": e.interprocedural,
                "min_confidence": e.min_confidence,
                "via_callee": e.via_callee,
            })
        })
        .collect();

    // Cyclic SCCs over the full graph (not just the focused view).
    let cycles = lock_order::find_lock_cycles(&edges, opts.max_cycle_len);
    let sccs: Vec<_> = cycles.iter().map(|c| json!(c.resources)).collect();

    let nodes_vec: Vec<&str> = nodes.into_iter().collect();
    json_result(&json!({
        "project": project,
        "nodes": nodes_vec,
        "edges": edges_json,
        "edge_count": view.len(),
        "cycles": sccs,
        "focus": focus,
        "max_call_depth": opts.max_call_depth,
        "confidence_floor": opts.confidence_floor,
        "guidance": "Directed lock-order graph from the sync_ops skeleton. An edge A→B means \
            B is acquired while A is held (intraprocedural or inlined across the call graph). \
            `cycles` are the SCCs = deadlock candidates (see deadlock_cycles for witnesses)."
    }))
}
