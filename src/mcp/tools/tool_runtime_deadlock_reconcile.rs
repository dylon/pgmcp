//! `tool_runtime_deadlock_reconcile` — reconcile a runtime trace against the
//! static lock-order graph (Dbg-1).
//!
//! The agent captures a trace where threads are blocked on locks (BCC
//! `offcputime -f` / `offwaketime` folded stacks, `perf script`, or `gdb thread
//! apply all bt`). This tool parses the blocked-on-lock wait relation, builds
//! the static interprocedural lock-order graph from the `sync_ops` skeleton
//! (the same engine behind `deadlock_cycles` / `lock_order_graph`), and
//! reconciles the two:
//!   - `confirmed`      — runtime waits the static analysis predicted.
//!   - `static_missed`  — runtime waits with NO static edge (precision gap).
//!   - `static_only`    — static edges this trace never exercised.
//!
//! Read-only: pgmcp parses the agent-provided trace text and runs SELECTs for
//! the static graph. It never attaches a debugger or runs perf.

use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use tracing::debug;

use crate::concurrency::trace_reconcile::{self, ObservedWait};
use crate::concurrency::{self, LockOrderOptions};
use crate::context::SystemContext;
use crate::graph::lock_order;
use crate::mcp::server::RuntimeDeadlockReconcileParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_runtime_deadlock_reconcile(
    ctx: &SystemContext,
    params: RuntimeDeadlockReconcileParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);

    let project = params.project.trim();
    if project.is_empty() {
        return Err(McpError::invalid_params("project must be non-empty", None));
    }
    let format = params.format.trim();
    if !matches!(format, "offcpu_folded" | "perf_script" | "gdb_bt") {
        return Err(McpError::invalid_params(
            format!("unknown format '{format}'; expected offcpu_folded | perf_script | gdb_bt"),
            None,
        ));
    }
    let confidence_floor = match params.confidence_floor {
        Some(v) if !v.is_finite() => {
            return Err(McpError::invalid_params(
                "confidence_floor must be finite",
                None,
            ));
        }
        Some(v) => v.clamp(0.0, 1.0),
        None => 0.3,
    };

    debug!(tool = "runtime_deadlock_reconcile", project, format, "MCP tool invoked");

    let pool = pool_or_err(ctx)?;
    let project_id = project_id_or_err(ctx, project).await?;

    // Parse the runtime trace into observed lock waits (pure text → structs).
    let observed: Vec<ObservedWait> = match format {
        "offcpu_folded" => trace_reconcile::parse_offcpu_folded(&params.trace_text),
        "perf_script" => trace_reconcile::parse_perf_script(&params.trace_text),
        "gdb_bt" => trace_reconcile::parse_gdb_bt(&params.trace_text),
        _ => Vec::new(),
    };

    // Build the static interprocedural lock-order graph.
    let opts = LockOrderOptions {
        max_call_depth: params.max_call_depth.unwrap_or(5).clamp(1, 12),
        confidence_floor,
        max_cycle_len: 6,
        call_confidence: 0.5,
    };
    let static_edges = concurrency::lock_order_edges(pool, project_id, opts)
        .await
        .map_err(|e| {
            McpError::internal_error(format!("static lock-order graph failed: {e}"), None)
        })?;
    let static_cycles = lock_order::find_lock_cycles(&static_edges, opts.max_cycle_len);

    // Reconcile. We drive the public pair-form `reconcile` entry (the canonical
    // API) from the parsed observations so the same code path serves an agent
    // that supplies pre-paired waits directly; the parsed `ObservedWait`s retain
    // the blocking-primitive witness we splice back onto each result below.
    let wait_pairs: Vec<(String, String)> = observed
        .iter()
        .map(|w| (w.holder.clone(), w.wanted.clone()))
        .collect();
    let rec = trace_reconcile::reconcile(&wait_pairs, &static_edges);

    // Map (holder,wanted) → the blocking primitive witnessed in the trace, so
    // each reconciled result can carry its diagnostic primitive even though the
    // pair-form `reconcile` API doesn't thread it through.
    let primitive_of = |holder: &str, wanted: &str| -> String {
        observed
            .iter()
            .find(|w| w.holder == holder && w.wanted == wanted)
            .map(|w| w.primitive.clone())
            .unwrap_or_default()
    };

    let confirmed: Vec<serde_json::Value> = rec
        .confirmed
        .iter()
        .map(|c| {
            json!({
                "holder": c.observed.holder,
                "wanted": c.observed.wanted,
                "primitive": primitive_of(&c.observed.holder, &c.observed.wanted),
                "static_from": c.static_from,
                "static_to": c.static_to,
                "interprocedural": c.interprocedural,
            })
        })
        .collect();
    let static_missed: Vec<serde_json::Value> = rec
        .static_missed
        .iter()
        .map(|w| {
            json!({
                "holder": w.holder,
                "wanted": w.wanted,
                "primitive": primitive_of(&w.holder, &w.wanted),
            })
        })
        .collect();
    let static_only: Vec<serde_json::Value> = rec
        .static_only
        .iter()
        .map(|e| {
            json!({
                "from": e.from,
                "to": e.to,
                "interprocedural": e.interprocedural,
            })
        })
        .collect();

    // Static cycles, annotated with whether the trace corroborated any of their
    // edges (a confirmed cycle is a runtime-witnessed deadlock candidate).
    let confirmed_resources: std::collections::HashSet<(String, String)> = rec
        .confirmed
        .iter()
        .map(|c| (c.static_from.clone(), c.static_to.clone()))
        .collect();
    let cycles_json: Vec<serde_json::Value> = static_cycles
        .iter()
        .map(|cyc| {
            let runtime_corroborated = cyc.edges.iter().any(|e| {
                let from = e.from.rsplit("::").next().unwrap_or(&e.from).to_string();
                let to = e.to.rsplit("::").next().unwrap_or(&e.to).to_string();
                confirmed_resources.contains(&(from, to))
            });
            json!({
                "resources": cyc.resources,
                "all_read": cyc.is_all_read(),
                "min_confidence": cyc.min_confidence(),
                "runtime_corroborated": runtime_corroborated,
            })
        })
        .collect();

    let graph_present = !static_edges.is_empty();

    let result = json!({
        "project": project,
        "format": format,
        "observed_waits": observed.len(),
        "static_edges": static_edges.len(),
        "static_cycles": cycles_json,
        "confirmed": confirmed,
        "static_missed": static_missed,
        "static_only": static_only,
        "summary": {
            "confirmed_count": rec.confirmed.len(),
            "static_missed_count": rec.static_missed.len(),
            "static_only_count": rec.static_only.len(),
        },
        "guidance": "Reconciles runtime lock-waits against the static lock-order graph. \
                     `confirmed` = the static analysis predicted this wait (trust the cycle). \
                     `static_missed` = a real runtime wait the static graph lacks — a precision \
                     gap (unresolved callee / dynamic dispatch / FFI); investigate those frames. \
                     `static_only` = static edges this workload didn't exercise (possible \
                     false-positive or untested path). A `runtime_corroborated` cycle is a \
                     deadlock candidate witnessed live — prioritize it.",
        "health": health_envelope(graph_present, !observed.is_empty()),
    });

    debug!(
        tool = "runtime_deadlock_reconcile",
        observed = observed.len(),
        confirmed = rec.confirmed.len(),
        static_missed = rec.static_missed.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    json_result(&result)
}

fn health_envelope(static_graph_present: bool, trace_had_waits: bool) -> serde_json::Value {
    json!({
        "static_graph_present": static_graph_present,
        "trace_had_lock_waits": trace_had_waits,
    })
}
