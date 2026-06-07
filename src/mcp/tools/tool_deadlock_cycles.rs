//! `tool_deadlock_cycles` — interprocedural lock-order cycle detection.
//!
//! Supersedes the *depth* of `deadlock_candidates` (which is an intra-function
//! regex heuristic): this builds the lock-order graph with lock identity from
//! the `sync_ops` skeleton, inlines callee-acquired locks across the resolved
//! call graph (RacerD-style deadlock domain), and reports SCC cycles with
//! witnessing call sites + severity. Soundness is proved in
//! `docs/formal/rocq/LockOrderDeadlock.v`.

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::concurrency::{self, LockOrderOptions};
use crate::context::SystemContext;
use crate::graph::lock_order::AcqMode;
use crate::mcp::server::DeadlockCyclesParams;
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

pub async fn tool_deadlock_cycles(
    ctx: &SystemContext,
    params: DeadlockCyclesParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "deadlock_cycles", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project = params.project.trim();
    let project_id = project_id_or_err(ctx, project).await?;
    let pool = pool_or_err(ctx)?;

    let opts = LockOrderOptions {
        max_call_depth: params.max_call_depth.unwrap_or(5).clamp(1, 12),
        confidence_floor: normalize_confidence_floor(params.confidence_floor)?,
        max_cycle_len: params.max_cycle_len.unwrap_or(6).clamp(2, 12) as usize,
        call_confidence: 0.5,
    };
    let include_low = params.include_low_confidence.unwrap_or(false);
    let limit = params.limit.unwrap_or(50).clamp(1, 500) as usize;

    let findings = concurrency::analyze_lock_order(pool, project_id, opts)
        .await
        .map_err(|e| McpError::internal_error(format!("lock-order analysis failed: {e}"), None))?;

    let mut cycles_json = Vec::new();
    for f in &findings {
        // Shared-read cycles cannot deadlock; surface only when explicitly asked.
        if f.cycle.is_all_read() && !include_low {
            continue;
        }
        let edges: Vec<_> = f
            .cycle
            .edges
            .iter()
            .map(|e| {
                let held = f.meta.get(&e.held_symbol);
                let acq = f.meta.get(&e.acquired_symbol);
                json!({
                    "from": e.from,
                    "to": e.to,
                    "from_mode": mode_str(e.from_mode),
                    "to_mode": mode_str(e.to_mode),
                    "interprocedural": e.interprocedural,
                    "min_confidence": e.min_confidence,
                    "held_at": held.map(|m| json!({
                        "symbol_id": e.held_symbol, "name": m.name,
                        "file": m.relative_path, "line": e.held_line,
                    })),
                    "acquired_at": acq.map(|m| json!({
                        "symbol_id": e.acquired_symbol, "name": m.name,
                        "file": m.relative_path, "line": e.acquired_line,
                    })),
                    "via_callee": e.via_callee,
                })
            })
            .collect();
        cycles_json.push(json!({
            "resources": f.cycle.resources,
            "edges": edges,
            "severity": f.severity.as_str(),
            "severity_score": f.score,
            "public_api_reachable": f.public_api_reachable,
            "cycle_len": f.cycle.resources.len(),
            "all_read": f.cycle.is_all_read(),
        }));
        if cycles_json.len() >= limit {
            break;
        }
    }

    json_result(&json!({
        "project": project,
        "deadlock_cycles": cycles_json,
        "returned": cycles_json.len(),
        "total_cycles": findings.len(),
        "limit": limit,
        "max_call_depth": opts.max_call_depth,
        "confidence_floor": opts.confidence_floor,
        "guidance": "Cycles in the interprocedural lock-order graph are Havender (1968) \
            circular-wait deadlock candidates; edge A→B means B is acquired while A is held. \
            An all-read rwlock cycle is informational (shown only with include_low_confidence). \
            Soundness (acyclic ⇒ deadlock-free) is proved in docs/formal/rocq/LockOrderDeadlock.v. \
            Resource identity is best-effort — discount low min_confidence."
    }))
}
