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
use tracing::{error, info};

use crate::db::queries;
use crate::stats::tracker::StatsTracker;

/// Refresh both unified-graph matviews (nodes then edges). Increments
/// `memory_graph_refreshes` on success.
pub async fn run_memory_graph_refresh(
    pool: &PgPool,
    stats: &StatsTracker,
) -> Result<(), sqlx::Error> {
    // Run both refreshes independently so a failure of one (e.g. a transient
    // lock) does not prevent the other from being refreshed.
    let nodes = queries::refresh_memory_unified_nodes(pool).await;
    let edges = queries::refresh_memory_unified_edges(pool).await;

    if let Err(e) = &nodes {
        error!(view = "memory_unified_nodes", error = %e, "matview refresh failed");
    }
    if let Err(e) = &edges {
        error!(view = "memory_unified_edges", error = %e, "matview refresh failed");
    }

    if nodes.is_ok() && edges.is_ok() {
        stats.memory_graph_refreshes.fetch_add(1, Ordering::Relaxed);
        info!("memory-graph-refresh: refreshed memory_unified_nodes + memory_unified_edges");
        Ok(())
    } else {
        // Surface the first error to `run_or_log`; the per-view warnings above
        // already pinpoint which refresh(es) failed.
        nodes.and(edges)
    }
}

/// Run the refresh, logging any error rather than panicking the cron thread.
pub async fn run_or_log(pool: Arc<PgPool>, stats: Arc<StatsTracker>) {
    if let Err(e) = run_memory_graph_refresh(&pool, &stats).await {
        error!(error = %e, "memory-graph-refresh pass failed");
    }
}
