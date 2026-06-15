//! `tool_cron_history` — MCP tool body: durable cron-run history from the
//! `cron_run_history` ledger (ADR-018). Returns a per-job rollup (last outcome,
//! last success, computed next-due, run/ok/fail/skip counts) plus a recent-runs
//! list with intrinsics (duration, RSS / thread deltas, job counters).
//!
//! Read-only: it only SELECTs from `cron_run_history`. Pairs with `index_stats`
//! (the in-memory latest-outcome snapshot) — this tool is the durable history
//! that survives restarts.

#![allow(unused_imports)]

use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use tracing::debug;

use crate::config::CronConfig;
use crate::context::SystemContext;
use crate::mcp::server::CronHistoryParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

/// Best-effort per-job interval (seconds) from `[cron]` config, for computing
/// `next_due = last_ok + interval`. Returns `None` for jobs without a fixed
/// recurring interval here (e.g. the deliberately fixed-stagger daemon crons),
/// in which case `next_due` is omitted rather than guessed.
fn cron_interval_secs(c: &CronConfig, job: &str) -> Option<u64> {
    let secs = match job {
        "stats-aggregation" => c.stats_aggregation_interval_secs,
        "stale-cleanup" => c.stale_cleanup_interval_secs,
        "integrity-check" => c.integrity_check_interval_secs,
        "db-maintenance" => c.db_maintenance_interval_secs,
        "git-history-index" => c.git_history_index_interval_secs,
        "similarity-scan" => c.similarity_scan_interval_secs,
        "semantic-edges" => c.semantic_edge_interval_secs,
        "graph-analysis" => c.graph_analysis_interval_secs,
        "symbol-extraction" => c.symbol_extraction_interval_secs,
        "function-metrics" => c.function_metrics_interval_secs,
        "call-graph" => c.call_graph_interval_secs,
        "code-raptor" => c.code_raptor_interval_secs,
        "fuzzy-sync" => c.fuzzy_sync_interval_secs,
        "ngram-lm-train" => c.ngram_lm_train_interval_secs,
        "topic-dendrogram" => c.topic_dendrogram_interval_secs,
        "topic-clustering" => c.topic_scan_interval_secs,
        "quality-history" => c.quality_history_interval_secs,
        "tool-policy-refresh" => c.tool_policy_interval_secs,
        "embedding-migration" => c.embedding_migration_interval_secs,
        "work-item-presence" => c.work_item_presence_interval_secs,
        "mcp-client-liveness" => c.mcp_client_liveness_interval_secs,
        "project-deps-index" => c.project_deps_index_interval_secs,
        "git-state-scan" => c.git_state_scan_interval_secs,
        "findings-promotion" => c.findings_promotion_interval_secs,
        "concurrency-scan" => c.concurrency_scan_interval_secs,
        "memory-graph-refresh" => c.memory_graph_refresh_interval_secs,
        _ => return None,
    };
    (secs > 0).then_some(secs)
}

pub async fn tool_cron_history(
    ctx: &SystemContext,
    params: CronHistoryParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    debug!(tool = "cron_history", "MCP tool invoked");

    let pool = pool_or_err(ctx)?;

    let job_filter = params
        .job
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let limit = params.limit.unwrap_or(50).clamp(1, 500);

    let rollup = crate::db::queries::cron_job_rollup(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("cron_job_rollup failed: {e}"), None))?;
    let recent = crate::db::queries::recent_cron_runs(pool, job_filter, limit)
        .await
        .map_err(|e| McpError::internal_error(format!("recent_cron_runs failed: {e}"), None))?;

    let cron_cfg = ctx.config().load().cron.clone();
    let by_job: Vec<serde_json::Value> = rollup
        .iter()
        .map(|r| {
            // next_due = last successful completion + the job's interval (when
            // the interval is known and the job has succeeded at least once).
            let next_due = match (r.last_ok, cron_interval_secs(&cron_cfg, &r.job_name)) {
                (Some(last_ok), Some(secs)) => last_ok
                    .checked_add_signed(chrono::Duration::seconds(secs as i64))
                    .map(|t| t.to_rfc3339()),
                _ => None,
            };
            json!({
                "job": r.job_name,
                "last_outcome": r.last_outcome,
                "last_completed_at": r.last_completed_at.to_rfc3339(),
                "last_ok": r.last_ok.map(|t| t.to_rfc3339()),
                "next_due": next_due,
                "run_count": r.run_count,
                "ok_count": r.ok_count,
                "fail_count": r.fail_count,
                "skip_count": r.skip_count,
            })
        })
        .collect();

    let result = json!({
        "by_job": by_job,
        "recent": recent,
        "recent_count": recent.len(),
        "recent_limit": limit,
        "job_filter": job_filter,
        "writes_dropped": ctx
            .stats()
            .cron_history_writes_dropped
            .load(Ordering::Relaxed),
        "guidance": "Durable cron-run history (cron_run_history, ADR-018). `by_job` is the per-job rollup (last outcome, last success, computed next_due, counts); `recent` lists individual runs with intrinsics (duration_ms, rss_mb_delta, threads_delta, counters). `writes_dropped` > 0 means the bounded writer channel overflowed (back-pressure). Pair with index_stats for the live in-memory latest-outcome snapshot.",
        "elapsed_ms": start.elapsed().as_millis() as u64,
    });
    json_result(&result)
}
