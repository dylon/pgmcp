//! `orchestration-session-reaper` cron — crash-resume for Crucible sessions
//! (ADR-009 PAUSE/RESUME).
//!
//! A single bounded UPDATE that auto-pauses every live (`running`/`resuming`)
//! `orchestration_sessions` row whose work-item lease has lapsed — i.e. the
//! orchestrator (pi) crashed mid-protocol. Without this sweep such a session
//! would sit `running` forever and never surface in `session_checkpoint_list`, so
//! no other agent could pick the run back up. Mirrors the work-item presence/lease
//! decay sweep: light, idempotent, order-free.
//!
//! Off by default (`[…] orchestration_session_reaper_interval_secs = 0`), like the
//! presence / csm-validate crons. The boundary is unchanged: this only writes
//! pgmcp's OWN table (flips `status` to `paused`), never the user's files.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use sqlx::PgPool;
use tracing::{error, info};

use crate::csm::session_store::expire_stale_to_paused;
use crate::stats::tracker::StatsTracker;

/// One reaper sweep. Light job (a single bounded UPDATE), so the scheduler runs
/// it on the runtime with no heavy-cron gate. Swallows + logs errors at `error!`
/// (ADR-021) so one bad tick does not kill the cron thread.
pub async fn run_or_log(pool: PgPool, stats: Arc<StatsTracker>) {
    stats.cron_executions.fetch_add(1, Ordering::Relaxed);
    match expire_stale_to_paused(&pool).await {
        Ok(paused) => {
            stats
                .orchestration_sessions_auto_paused
                .fetch_add(paused, Ordering::Relaxed);
            if paused > 0 {
                info!(
                    paused,
                    "orchestration-session-reaper cron: auto-paused crashed sessions (lease lapsed)"
                );
            }
        }
        Err(e) => {
            stats.cron_panics.fetch_add(1, Ordering::Relaxed);
            error!(error = %e, "orchestration-session-reaper cron: sweep failed");
        }
    }
}
