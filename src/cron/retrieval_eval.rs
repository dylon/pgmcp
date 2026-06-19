//! Retrieval-quality drift cron.
//!
//! Periodically scores the frozen probe set
//! ([`crate::quality::retrieval_drift`]) through `semantic_search` and persists
//! the report into `pgmcp_metadata` under `retrieval_eval_last_report`, warning
//! when quality falls below the floor (a likely regression: a broken embedder, a
//! botched migration, a dropped HNSW index). This is the **runtime** complement
//! to the CI regression gate (`pgmcp-testing/tests/eval_semantic_quality.rs`),
//! which catches the same collapse at push time.
//!
//! Default is **off** (`[cron] retrieval_eval_interval_secs = 0`) — like the
//! memory-eval cron it is opportunistic regression detection, not a gate.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use sqlx::PgPool;
use tracing::{error, info, warn};

use crate::embed::EmbedSource;
use crate::quality::retrieval_drift::run_retrieval_drift;
use crate::stats::tracker::StatsTracker;

/// Conservative pass floors. The live-corpus baseline is MRR ≈ 0.30 /
/// recall@10 ≈ 0.74 (see `docs/evaluation/semantic-search-quality.md`); the
/// floors sit far below to tolerate ±0.02 non-stationary drift and only trip on
/// a collapse.
const MIN_MRR: f64 = 0.15;
const MIN_RECALL_AT_10: f64 = 0.45;

pub async fn run_or_log(pool: PgPool, embed: EmbedSource, stats: Arc<StatsTracker>, project: &str) {
    let _ = stats.cron_executions.fetch_add(1, Ordering::Relaxed);
    let report = run_retrieval_drift(&pool, &embed, project).await;
    let passed = report.meets_floor(MIN_MRR, MIN_RECALL_AT_10);
    let payload = serde_json::json!({
        "project": report.project,
        "n_probes": report.n_probes,
        "n_scored": report.n_scored,
        "mrr": report.mrr,
        "recall_at_10": report.recall_at_10,
        "success_at_1": report.success_at_1,
        "failures": report.failures,
        "passed": passed,
        "min_mrr": MIN_MRR,
        "min_recall_at_10": MIN_RECALL_AT_10,
    });
    if let Err(e) = sqlx::query(
        "INSERT INTO pgmcp_metadata (key, value) VALUES ('retrieval_eval_last_report', $1) \
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(payload.to_string())
    .execute(&pool)
    .await
    {
        stats.cron_panics.fetch_add(1, Ordering::Relaxed);
        error!(error = %e, "retrieval-eval cron: failed to persist report");
        return;
    }
    if !passed {
        warn!(
            mrr = report.mrr,
            recall_at_10 = report.recall_at_10,
            n_scored = report.n_scored,
            failures = report.failures,
            "retrieval-eval cron: quality BELOW floor (possible retrieval regression)"
        );
    } else {
        info!(
            mrr = report.mrr,
            recall_at_10 = report.recall_at_10,
            n_scored = report.n_scored,
            "retrieval-eval cron: retrieval quality healthy"
        );
    }
}
