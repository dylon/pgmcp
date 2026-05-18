//! `telemetry-retention` cron job.
//!
//! Daily DELETE pass over `mcp_tool_calls` that drops rows older than
//! `MetricsConfig::telemetry_retention_days`. Runs as a recurring job
//! scheduled from `src/cli/daemon.rs`; non-blocking on the cron poll
//! thread because the actual work runs on the cron `WorkPool` via a
//! `tokio::runtime::Handle`.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use sqlx::PgPool;
use tracing::{debug, info, warn};

use crate::stats::tracker::StatsTracker;

/// Delete `mcp_tool_calls` rows older than `retention_days`. Returns the
/// number of rows removed. Caller is responsible for incrementing
/// `telemetry_rows_purged`; this function does so directly.
pub async fn run_telemetry_retention(
    pool: &PgPool,
    stats: &StatsTracker,
    retention_days: u32,
) -> Result<u64, sqlx::Error> {
    let days = retention_days.max(1) as i32;
    debug!(retention_days = days, "telemetry-retention pass starting");
    let res =
        sqlx::query("DELETE FROM mcp_tool_calls WHERE ts < now() - ($1::int * interval '1 day')")
            .bind(days)
            .execute(pool)
            .await?;
    let removed = res.rows_affected();
    if removed > 0 {
        info!(
            rows_removed = removed,
            retention_days = days,
            "telemetry-retention purged"
        );
    } else {
        debug!("telemetry-retention: no rows to purge");
    }
    stats
        .telemetry_rows_purged
        .fetch_add(removed, Ordering::Relaxed);
    Ok(removed)
}

/// Run the retention pass, logging any error rather than panicking the
/// cron thread.
pub async fn run_or_log(pool: Arc<PgPool>, stats: Arc<StatsTracker>, retention_days: u32) {
    if let Err(e) = run_telemetry_retention(&pool, &stats, retention_days).await {
        warn!(error = %e, "telemetry-retention pass failed");
    }
}
