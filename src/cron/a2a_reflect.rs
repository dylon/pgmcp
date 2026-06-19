//! Part A phase A4: cross-agent best-practice reflection + promotion cron.
//!
//! Off by default (`[a2a.reflection] cron_enabled = false`). Consensus-gates
//! peer outcomes into the shared scope, optionally LLM-reflects each touched
//! scope, and promotes the strongest agreed practices to durable mandates.
//! Modeled on `src/cron/memory_reflect.rs`.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use sqlx::PgPool;
use tracing::{error, info};

use crate::config::A2aReflectionConfig;
use crate::llm::LlmExtractor;
use crate::stats::tracker::StatsTracker;

/// Daemon-facing entry point. Swallows + logs errors so one bad tick does
/// not kill the cron thread.
pub async fn run_or_log(
    pool: Arc<PgPool>,
    stats: Arc<StatsTracker>,
    extractor: Option<Arc<dyn LlmExtractor>>,
    cfg: A2aReflectionConfig,
) {
    stats.cron_executions.fetch_add(1, Ordering::Relaxed);
    match crate::a2a::best_practices::run_cross_agent_reflection(
        &pool,
        &stats,
        extractor.as_deref(),
        &cfg,
    )
    .await
    {
        Ok(report) => info!(
            consensus_groups = report.consensus_groups,
            scopes_reflected = report.scopes_reflected,
            mandates_promoted = report.mandates_promoted,
            "a2a-reflect cron completed"
        ),
        Err(e) => error!(error = %e, "a2a-reflect cron failed"),
    }
}
