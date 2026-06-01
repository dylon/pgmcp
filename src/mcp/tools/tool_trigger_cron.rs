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
            // Manual trigger: no general WorkPool in scope, so betweenness runs
            // sequentially (gated by DENSE_CENTRALITY_MAX_NODES in the cron).
            crate::cron::call_graph::run_call_graph(db.as_ref(), stats, None).await;
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
        "a2a-reflect" => {
            // Part A phase A4: consensus-gate peer outcomes into the shared
            // scope and promote the strongest agreed practices to durable
            // mandates. On-demand counterpart to the off-by-default cron.
            let pool = ctx
                .db()
                .pool()
                .ok_or_else(|| McpError::internal_error("no pool available", None))?;
            let cfg = ctx.config().load().a2a.reflection.clone();
            let extractor = ctx.llm_extractor();
            let report = crate::a2a::best_practices::run_cross_agent_reflection(
                pool,
                stats,
                extractor.as_deref(),
                &cfg,
            )
            .await
            .map_err(|e| McpError::internal_error(format!("a2a-reflect failed: {e}"), None))?;
            json_result(&json!({
                "job": params.job,
                "status": "completed",
                "consensus_groups": report.consensus_groups,
                "scopes_reflected": report.scopes_reflected,
                "mandates_promoted": report.mandates_promoted,
                "guidance": "Agreed peer best practices promoted to durable mandates; they re-inject via the UserPromptSubmit hook.",
            }))
        }
        "msm-calibrate" => {
            // Part E (closed MSM loop): refresh trajectory success labels from
            // explicit outcomes, then re-tune the adaptive split/merge cost c
            // for cohort separation (LOO precision-guarded) and persist it.
            let pool = ctx
                .db()
                .pool()
                .ok_or_else(|| McpError::internal_error("no pool available", None))?;
            use crate::fuzzy::trajectory_index::{
                DEFAULT_MSM_C, calibrate_adaptive_c, label_trajectories_from_outcomes, load_msm_c,
                loo_accuracy, store_msm_c,
            };
            let labeled = label_trajectories_from_outcomes(pool)
                .await
                .map_err(|e| McpError::internal_error(format!("label step: {e}"), None))?;
            let cohort = |success: bool| async move {
                sqlx::query_as::<_, (i64, Vec<f64>)>(
                    "SELECT id, encoded_series FROM agent_trajectories
                     WHERE success = $1 AND cardinality(encoded_series) > 0",
                )
                .bind(success)
                .fetch_all(pool)
                .await
            };
            let success = cohort(true)
                .await
                .map_err(|e| McpError::internal_error(format!("success cohort: {e}"), None))?;
            let fail = cohort(false)
                .await
                .map_err(|e| McpError::internal_error(format!("fail cohort: {e}"), None))?;
            let prev_c = load_msm_c(pool).await.unwrap_or(DEFAULT_MSM_C);
            let new_c = calibrate_adaptive_c(&success, &fail, prev_c, 64);
            let _ = store_msm_c(pool, new_c).await;
            json_result(&json!({
                "job": params.job,
                "status": "completed",
                "newly_labeled": labeled,
                "success_cohort": success.len(),
                "fail_cohort": fail.len(),
                "previous_c": prev_c,
                "calibrated_c": new_c,
                "loo_accuracy": loo_accuracy(&success, &fail, new_c),
                "guidance": "Adaptive MSM cost re-tuned for cohort separation; the RLM strategy chooser (a2a_pattern_recursive) now uses it.",
            }))
        }
        "fuzzy-sync" => {
            // Rebuild the per-project symbol/path/commit + durable-mandate fuzzy
            // tries from PostgreSQL — the on-demand counterpart to the fuzzy-sync
            // cron. Clone config values before the await so the ArcSwap guard is
            // not held across it.
            let pool = ctx
                .db()
                .pool()
                .ok_or_else(|| McpError::internal_error("no pool available", None))?;
            let (data_dir, max_disk_bytes, eviction_cfg) = {
                let cfg = ctx.config().load();
                (
                    cfg.fuzzy.data_dir.clone(),
                    cfg.fuzzy.max_disk_bytes,
                    cfg.fuzzy.eviction_config(),
                )
            };
            let report = crate::cron::fuzzy_sync::run_fuzzy_sync(
                pool,
                &data_dir,
                max_disk_bytes,
                eviction_cfg,
                std::sync::Arc::clone(stats),
            )
            .await
            .map_err(|e| McpError::internal_error(format!("fuzzy-sync failed: {e}"), None))?;
            json_result(&json!({
                "job": params.job,
                "status": "completed",
                "symbols_synced": report.symbols_synced,
                "paths_synced": report.paths_synced,
                "commits_synced": report.commits_synced,
                "durable_mandates_synced": report.durable_mandates_synced,
                "guidance": "Per-project symbol/path/commit + durable-mandate fuzzy tries rebuilt from PG.",
            }))
        }
        "graph-analysis" => {
            // Rebuild code_graph_edges (import / co-change / semantic) on demand.
            // Run AFTER symbol-extraction so the freshly written `import_use`
            // refs materialize into import edges — this is how the post-fix
            // import-graph backfill is forced without a daemon restart.
            crate::cron::graph_analysis::run_graph_analysis(db.as_ref(), stats, None).await;
            json_result(&json!({
                "job": params.job,
                "status": "completed",
                "guidance": "Import/co-change/semantic edges rebuilt from symbol_references. Repairs dependency_graph / coupling_cohesion_report / architecture_* once import_use refs exist (run symbol-extraction first).",
            }))
        }
        other => Err(McpError::invalid_params(
            format!(
                "Unknown job {other:?}. Valid: symbol-extraction | call-graph | function-metrics | graph-analysis | a2a-reflect | msm-calibrate | fuzzy-sync"
            ),
            None,
        )),
    }
}
