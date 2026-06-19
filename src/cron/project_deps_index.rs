//! Project-dependency index cron. Periodically re-parses every project's Cargo
//! manifests into `project_dependencies` (source=cargo), upserting live edges
//! and closing vanished ones (bitemporal). The resulting `project_depends_on`
//! unified-graph edges are picked up by the next `memory-graph-refresh`. Light
//! job (bounded file walks + per-edge upserts); modeled on `work_item_presence`.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use sqlx::PgPool;
use tracing::{error, info};

use crate::stats::tracker::StatsTracker;

/// One manifest-indexing pass over all projects.
pub async fn run_or_log(pool: PgPool, stats: Arc<StatsTracker>) {
    let _ = stats.cron_executions.fetch_add(1, Ordering::Relaxed);
    let projects: Vec<(i32, String)> = match sqlx::query_as("SELECT id, path FROM projects")
        .fetch_all(&pool)
        .await
    {
        Ok(p) => p,
        Err(e) => {
            stats.cron_panics.fetch_add(1, Ordering::Relaxed);
            error!(error = %e, "project-deps-index cron: project list failed");
            return;
        }
    };

    let mut total_up = 0usize;
    let mut total_closed = 0u64;
    for (id, path) in projects {
        let (up, closed) = crate::deps::ecosystems::index_all_manifests(&pool, id, &path).await;
        total_up += up;
        total_closed += closed;
    }
    if total_up > 0 || total_closed > 0 {
        info!(
            upserted = total_up,
            closed = total_closed,
            "project-deps-index cron: indexed manifest dependencies"
        );
    }
}
