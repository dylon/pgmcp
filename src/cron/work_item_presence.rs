//! Work-item presence/lease decay cron.
//!
//! Periodic sweep that (1) releases expired claims (lease-crash-safety —
//! NULLing `work_items.claimed_by` with an `expire` ledger row) and (2) decays
//! `agent_presence` (active→idle→offline). Copies the `memory_retention` cron
//! shape. Light job (no `cron_pool`/heavy gate) — a couple of bounded UPDATEs.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use sqlx::PgPool;
use tracing::{info, warn};

use crate::db::queries;
use crate::stats::tracker::StatsTracker;

/// One presence/lease sweep. `pool` is an owned `PgPool` (cheaply cloned from
/// `DbClient::pool()` — it is `Arc`-backed internally). Light job: a couple of
/// bounded UPDATEs, so the scheduler runs it unconditionally on the runtime
/// (no heavy-cron gate / `cron_pool` submission).
pub async fn run_or_log(pool: PgPool, stats: Arc<StatsTracker>, idle_secs: i64, offline_secs: i64) {
    let _ = stats.cron_executions.fetch_add(1, Ordering::Relaxed);
    stats.presence_sweeps.fetch_add(1, Ordering::Relaxed);
    match queries::sweep_presence_and_leases(&pool, idle_secs, offline_secs).await {
        Ok((leases_expired, presence_idled)) => {
            stats
                .work_item_leases_expired
                .fetch_add(leases_expired, Ordering::Relaxed);
            if leases_expired + presence_idled > 0 {
                info!(
                    leases_expired,
                    presence_idled, "work-item-presence cron: swept stale leases + presence"
                );
            }
        }
        Err(e) => {
            stats.cron_panics.fetch_add(1, Ordering::Relaxed);
            warn!(error = %e, "work-item-presence cron: sweep failed");
        }
    }
}
