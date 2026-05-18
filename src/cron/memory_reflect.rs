//! Memory-server Phase 5: reflection cron job.
//!
//! Periodic background pass that calls
//! `llm::reflect::run_reflection_cron` for every scope crossing the
//! `[memory.reflection] min_new_observations` threshold. Off by default
//! — the operator opts in via `[memory.reflection] cron_enabled = true`.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use sqlx::PgPool;
use tracing::warn;

use crate::llm::LlmExtractor;
use crate::stats::tracker::StatsTracker;

/// Daemon-facing entry point. Swallow + log errors so a single bad tick
/// doesn't kill the cron thread.
pub async fn run_or_log(
    pool: Arc<PgPool>,
    stats: Arc<StatsTracker>,
    extractor: Arc<dyn LlmExtractor>,
    min_new_observations: i64,
    max_observations: i64,
) {
    let _ = stats.cron_executions.fetch_add(1, Ordering::Relaxed);
    let pool_for_cron = pool.clone();
    let stats_for_cron = stats.clone();
    let extractor_for_cron = extractor;
    crate::llm::reflect::run_reflection_cron(
        pool_for_cron,
        stats_for_cron.clone(),
        extractor_for_cron,
        min_new_observations,
        max_observations,
    )
    .await;
    if stats_for_cron
        .memory_reflection_errors
        .load(Ordering::Acquire)
        > 0
    {
        warn!("memory_reflect cron: at least one scope reflection errored — see warnings above");
    }
}
