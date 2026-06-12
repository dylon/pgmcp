//! The DB-availability prober — the **single writer** of [`DbHealth`].
//!
//! A lightweight background loop that runs `SELECT 1` on a cadence and flips
//! the shared breaker. It is the only place that logs DB up/down state, and it
//! logs **only on transitions**, collapsing what used to be a per-operation
//! flood (1447 lines for the 2026-06-11 outage) to two lines per outage. On the
//! Down→Up edge it kicks the [`OutboxReplayer`] so spooled ephemeral events are
//! re-POSTed.

use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::health::outbox::OutboxReplayer;
use crate::stats::tracker::StatsTracker;

/// Spawn the prober loop. `interval_secs` is the steady-state cadence;
/// `probe_timeout_secs` bounds each probe so a hung pool cannot make the prober
/// sit on the full `acquire_timeout` (10 s) every cycle — an elapsed probe is
/// treated as a failure observation.
pub fn spawn_db_prober(
    pool: PgPool,
    stats: Arc<StatsTracker>,
    replayer: Option<Arc<OutboxReplayer>>,
    interval_secs: u64,
    probe_timeout_secs: u64,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let interval = Duration::from_secs(interval_secs.max(1));
        let probe_timeout = Duration::from_secs(probe_timeout_secs.max(1));
        info!(
            interval_secs = interval.as_secs(),
            probe_timeout_secs = probe_timeout.as_secs(),
            "db-prober: started"
        );
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(interval) => {}
            }

            let probe =
                tokio::time::timeout(probe_timeout, crate::db::pool::health_check(&pool)).await;

            match probe {
                Ok(Ok(())) => {
                    if let Some(down_for) = stats.db_health().record_success() {
                        info!(
                            down_seconds = down_for,
                            "database recovered after {down_for}s"
                        );
                        // Re-POST anything spooled during the outage.
                        if let Some(r) = replayer.clone() {
                            tokio::spawn(async move {
                                r.replay().await;
                            });
                        }
                    }
                }
                Ok(Err(e)) => {
                    if stats.db_health().record_failure() {
                        error!(error = %e, "database unreachable (probe failed); pausing DB work");
                    }
                }
                Err(_elapsed) => {
                    if stats.db_health().record_failure() {
                        error!(
                            timeout_secs = probe_timeout.as_secs(),
                            "database unreachable (probe timed out); pausing DB work"
                        );
                    }
                }
            }
        }
        info!("db-prober: stopped");
    })
}
