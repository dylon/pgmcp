//! `csm-validate` cron — auto-conformance for A2A pattern runs (ADR-009).
//!
//! Scans completed `a2a_pattern_*` runs that have no `csm_run_traces` row yet
//! and validates each, feeding the MSM learner. This closes the CSM learning
//! loop WITHOUT depending on an agent calling `csm_validate_run` (agents never
//! do — CSM sits at zero usage). LLM-free: the conformance verdict is pure
//! (`csm::validate::prepare_validation`), so no extractor is needed. Off by
//! default (`[a2a.csm_validate] cron_enabled = false`). Modeled on
//! `cron::a2a_reflect`.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use sqlx::PgPool;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::config::A2aCsmValidateConfig;
use crate::csm::store::insert_run_trace_if_absent;
use crate::csm::validate::{Prepared, prepare_validation};
use crate::stats::tracker::StatsTracker;

/// Daemon-facing entry point. Swallows + logs errors so one bad tick does not
/// kill the cron thread.
pub async fn run_or_log(pool: Arc<PgPool>, stats: Arc<StatsTracker>, cfg: A2aCsmValidateConfig) {
    stats.cron_executions.fetch_add(1, Ordering::Relaxed);
    match run(&pool, cfg.batch_limit).await {
        Ok(validated) => info!(validated, "csm-validate cron completed"),
        Err(e) => warn!(error = %e, "csm-validate cron failed"),
    }
}

/// Validate up to `limit` un-traced completed pattern runs. Returns the number
/// of new run-trace rows written.
async fn run(pool: &PgPool, limit: i64) -> Result<usize, String> {
    let limit = limit.clamp(1, 10_000);
    // Coarse pre-filter (status + skill prefix + no existing trace); the precise
    // skill check happens in `prepare_validation`, which skips non-patterns.
    let task_ids: Vec<Uuid> = sqlx::query_scalar(
        r"SELECT id FROM a2a_tasks
          WHERE skill_id LIKE 'a2a\_pattern\_%'
            AND status = 'completed'
            AND NOT EXISTS (SELECT 1 FROM csm_run_traces t WHERE t.task_id = a2a_tasks.id)
          ORDER BY id
          LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("scan failed: {e}"))?;

    let mut validated = 0usize;
    for task_id in task_ids {
        match prepare_validation(pool, task_id).await {
            Ok(Prepared::Ready(r)) => {
                match insert_run_trace_if_absent(
                    pool,
                    task_id,
                    r.protocol.name(),
                    r.conformant,
                    r.conformance_error.as_deref(),
                    &r.trace,
                    &r.encoded,
                    r.trajectory_id,
                )
                .await
                {
                    Ok(Some(_)) => validated += 1,
                    // Raced with a manual csm_validate_run / another tick — fine.
                    Ok(None) => {}
                    Err(e) => warn!(%task_id, error = %e, "csm-validate insert failed"),
                }
            }
            Ok(Prepared::Skip { reason, .. }) => {
                debug!(%task_id, reason, "csm-validate skipped (not validatable)")
            }
            Ok(Prepared::NotFound) => {}
            Err(e) => warn!(%task_id, error = %e, "csm-validate prepare failed"),
        }
    }
    Ok(validated)
}
