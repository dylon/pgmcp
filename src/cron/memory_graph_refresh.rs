//! `memory-graph-refresh` cron job.
//!
//! Refreshes the materialized `memory_unified_nodes` and `memory_unified_edges`
//! views so the heterogeneous knowledge graph the traversal tools
//! (`memory_neighbors`, `memory_path_search`, `graph_neighbors`) walk stays
//! current with the indexed corpus. Both are cheap UNION-ALL projections.
//!
//! This wires up the previously-never-called `refresh_memory_unified_nodes`
//! (and the new `refresh_memory_unified_edges`): before this job the matviews
//! were built once at boot (on a hash change) and then frozen. Scheduled from
//! `src/cli/daemon.rs`; non-blocking on the cron poll thread (work runs on the
//! cron `WorkPool` via a `tokio::runtime::Handle`).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use sqlx::PgPool;
use tracing::{info, warn};

use crate::db::queries;
use crate::stats::tracker::StatsTracker;

/// Refresh both unified-graph matviews (nodes then edges). Increments
/// `memory_graph_refreshes` on success.
pub async fn run_memory_graph_refresh(
    pool: &PgPool,
    stats: &StatsTracker,
) -> Result<(), sqlx::Error> {
    queries::refresh_memory_unified_nodes(pool).await?;
    queries::refresh_memory_unified_edges(pool).await?;
    stats.memory_graph_refreshes.fetch_add(1, Ordering::Relaxed);
    info!("memory-graph-refresh: refreshed memory_unified_nodes + memory_unified_edges");
    Ok(())
}

/// Run the refresh, logging any error rather than panicking the cron thread.
pub async fn run_or_log(pool: Arc<PgPool>, stats: Arc<StatsTracker>) {
    if let Err(e) = run_memory_graph_refresh(&pool, &stats).await {
        warn!(error = %e, "memory-graph-refresh pass failed");
    }
}
