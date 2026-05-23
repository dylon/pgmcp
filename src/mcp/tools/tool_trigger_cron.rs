//! `trigger_cron` — operator-facing on-demand cron run.
//!
//! The heavy crons (symbol-extraction, call-graph, function-metrics)
//! have a Ready-relative delay (default 30 min) and a steady-state
//! interval (default 2 h). Freshly-started daemons therefore return
//! empty results from `dead_code_reachability` / `naming_consistency`
//! until that delay elapses. This tool lets the operator trigger an
//! immediate run when the data is needed sooner.
//!
//! Safety: each invocation acquires the heavy-cron lock non-blocking
//! (`try_lock`). If a heavy cron is already executing, the call returns
//! `{ status: "busy", retry_after_secs: 60 }` rather than queueing.
//! There's no rate limiting beyond that — heavy crons are themselves
//! the bottleneck and the lock already serialises them.

#![allow(unused_imports)]

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::server::TriggerCronParams;
use crate::mcp::tools::sota_helpers::json_result;

pub async fn tool_trigger_cron(
    ctx: &SystemContext,
    params: TriggerCronParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "trigger_cron", job = %params.job, "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);

    let db = ctx.db();
    let stats = ctx.stats();

    match params.job.as_str() {
        "symbol-extraction" => {
            crate::cron::symbol_extraction::run_symbol_extraction(db.as_ref(), stats).await;
            json_result(&json!({
                "job": params.job,
                "status": "completed",
                "guidance": "Symbols populated. dead_code_reachability and naming_consistency should now return populated results. For end-to-end call-graph closure, also run trigger_cron job=\"call-graph\".",
            }))
        }
        "call-graph" => {
            crate::cron::call_graph::run_call_graph(db.as_ref(), stats).await;
            json_result(&json!({
                "job": params.job,
                "status": "completed",
                "guidance": "Call graph populated. dead_code_reachability now uses real symbol_references edges.",
            }))
        }
        "function-metrics" => {
            crate::cron::function_metrics::run_function_metrics(db.as_ref(), stats).await;
            // Shadow-ASR channel (Phase D2b): workspace-wide effect distribution.
            let effect_breakdown: Vec<serde_json::Value> = (async {
                let Some(pool) = ctx.db().pool() else {
                    return Vec::new();
                };
                let rows: Vec<(String, i64)> = sqlx::query_as(
                    "SELECT se.effect, COUNT(*)::int8
             FROM symbol_effects se
             GROUP BY se.effect
             ORDER BY se.effect",
                )
                .fetch_all(pool)
                .await
                .unwrap_or_default();
                rows.into_iter()
                    .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
                    .collect()
            })
            .await;

            json_result(&json!({
            "effect_breakdown": effect_breakdown,
                    "job": params.job,
                    "status": "completed",
                    "guidance": "Function metrics populated (cyclomatic, cognitive, Halstead, NPath, MI).",
                }))
        }
        other => Err(McpError::invalid_params(
            format!(
                "Unknown job {other:?}. Valid: symbol-extraction | call-graph | function-metrics"
            ),
            None,
        )),
    }
}
