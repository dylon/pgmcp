//! Memory-server Phase 8.2: retention cron.
//!
//! Periodic hard-delete pass over `memory_entities`, `memory_observations`,
//! and `memory_relations` rows that are:
//!   1. soft-deleted (`valid_to IS NOT NULL`),
//!   2. older than `[memory.retention] window_days`,
//!   3. below `importance_threshold`, and
//!   4. not pointed at by any `superseded_by` chain root.
//!
//! Defaults to **on**; the only "destructive" cron in the memory-server
//! stack. Operators who don't want any hard deletion set
//! `[memory.retention] enabled = false`.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use sqlx::PgPool;
use tracing::{error, info};

use crate::db::queries;
use crate::stats::tracker::StatsTracker;

pub async fn run_or_log(
    pool: Arc<PgPool>,
    stats: Arc<StatsTracker>,
    window_days: i64,
    importance_threshold: f32,
) {
    let _ = stats.cron_executions.fetch_add(1, Ordering::Relaxed);
    match queries::memory_retention_purge(&pool, window_days, importance_threshold).await {
        Ok((e, o, r)) => {
            stats
                .memory_retention_entities_purged
                .fetch_add(e, Ordering::Relaxed);
            stats
                .memory_retention_observations_purged
                .fetch_add(o, Ordering::Relaxed);
            stats
                .memory_retention_relations_purged
                .fetch_add(r, Ordering::Relaxed);
            if e + o + r > 0 {
                info!(
                    entities_deleted = e,
                    observations_deleted = o,
                    relations_deleted = r,
                    window_days,
                    importance_threshold,
                    "memory-retention cron: purged"
                );
            }
        }
        Err(e) => {
            stats.cron_panics.fetch_add(1, Ordering::Relaxed);
            error!(error = %e, "memory-retention cron: purge failed");
        }
    }
}
