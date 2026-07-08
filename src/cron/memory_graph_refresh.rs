//! `memory-graph-refresh` cron job.
//!
//! Refreshes the materialized `memory_unified_nodes` and `memory_unified_edges`
//! views so the heterogeneous knowledge graph the traversal tools
//! (`memory_neighbors`, `memory_path_search`, `graph_neighbors`) walk stays
//! current with the indexed corpus.
//!
//! Each `REFRESH MATERIALIZED VIEW CONCURRENTLY` rebuilds the whole matview and
//! re-maintains its indexes; for `memory_unified_nodes` that includes a multi-GB
//! HNSW vector index over ~1M rows, so a refresh saturates PostgreSQL for ~10 min
//! and backs up pgmcp's own in-flight heap — the balloon behind the repeated OOM
//! kills of 2026-07-06. Two changes keep that in check here:
//!   1. a **data-change gate** — skip the refresh entirely when the corpus is
//!      unchanged since the last successful refresh (most 6-hourly runs), bounded
//!      by a max-staleness backstop so low-churn non-file arms still appear; and
//!   2. registration through the **gated heavy-cron path** (serialized +
//!      memory-pressure gate + post-body `malloc_trim`), in `src/cron/scheduler.rs`
//!      (`schedule_maintenance_jobs`), instead of the old ungated light cron.

use sqlx::PgPool;
use std::sync::atomic::Ordering;
use tracing::{error, info};

use crate::db::queries;
use crate::stats::tracker::StatsTracker;

/// `pgmcp_metadata` key holding the last successful nodes+edges (structural)
/// refresh watermark, `"{corpus_fingerprint}|{epoch_secs}"`.
const WATERMARK_KEY: &str = "memory_graph_refresh_watermark";

/// `pgmcp_metadata` key holding the last successful vectors-matview refresh
/// watermark — a SEPARATE cadence from the structural nodes+edges refresh, so the
/// expensive HNSW re-maintenance can run less often (`memory-vectors-refresh`).
const VECTORS_WATERMARK_KEY: &str = "memory_vectors_refresh_watermark";

/// Outcome of a refresh pass — lets the heavy cron record `NoOp` (skipped, the
/// corpus was unchanged) vs `Ok` (actually refreshed), so an unchanged pass shows
/// a ~0 RSS delta in `cron_run_history`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshOutcome {
    Refreshed,
    SkippedUnchanged,
}

/// Cheap corpus fingerprint + the DB clock, in one round-trip. `file_chunks`
/// (`count` + `max(id)`) advances on every file (re)index — the `DELETE`+`INSERT`
/// chunk churn in `replace_indexed_file` — so it tracks the dominant, high-churn
/// source feeding the unified matviews. Low-churn non-file arms (entities /
/// work-items created directly by agents) are covered by the `max_staleness_secs`
/// backstop, not by this fingerprint.
async fn corpus_fingerprint(pool: &PgPool) -> Result<(String, i64), sqlx::Error> {
    let row: (String, i64) = sqlx::query_as(
        "SELECT count(*)::text || ':' || coalesce(max(id), 0)::text,
                extract(epoch FROM now())::bigint
           FROM file_chunks",
    )
    .fetch_one(pool)
    .await?;
    Ok(row)
}

async fn read_watermark(pool: &PgPool, key: &str) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar::<_, String>("SELECT value FROM pgmcp_metadata WHERE key = $1")
        .bind(key)
        .fetch_optional(pool)
        .await
}

async fn write_watermark(
    pool: &PgPool,
    key: &str,
    fingerprint: &str,
    at: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO pgmcp_metadata (key, value) VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(key)
    .bind(format!("{fingerprint}|{at}"))
    .execute(pool)
    .await?;
    Ok(())
}

/// Pure gate decision (testable without a DB): the corpus counts as unchanged
/// iff the stored fingerprint matches the current one **and** the last refresh is
/// younger than `max_staleness_secs`. A malformed / missing watermark, a
/// fingerprint mismatch, or an expired watermark all fall through to a refresh.
fn is_unchanged(
    stored: Option<&str>,
    fingerprint: &str,
    now: i64,
    max_staleness_secs: u64,
) -> bool {
    let Some(stored) = stored else {
        return false;
    };
    let Some((last_fp, last_at)) = stored.split_once('|') else {
        return false;
    };
    if last_fp != fingerprint {
        return false;
    }
    let Ok(last_at) = last_at.parse::<i64>() else {
        return false;
    };
    now.saturating_sub(last_at) < max_staleness_secs as i64
}

/// Refresh the STRUCTURAL matviews — `memory_unified_nodes` + `memory_unified_edges`,
/// the cheap graph the traversal tools (`memory_neighbors` / `memory_path_search` /
/// `graph_neighbors`) walk — gated on a corpus data-change check. The expensive
/// vectors/HNSW matview is refreshed SEPARATELY by [`run_memory_vectors_refresh`]
/// on its own (slower) cadence, so the traversal graph can stay fresh at a short
/// interval (e.g. every 5 min) without paying the ~minutes-long HNSW re-maintenance
/// every tick. Returns [`RefreshOutcome::SkippedUnchanged`] without touching the
/// matviews when the corpus is unchanged and the last refresh is within
/// `max_staleness_secs`; otherwise refreshes both and stamps the watermark.
pub async fn run_memory_graph_refresh(
    pool: &PgPool,
    stats: &StatsTracker,
    max_staleness_secs: u64,
) -> Result<RefreshOutcome, sqlx::Error> {
    let (fingerprint, now) = corpus_fingerprint(pool).await?;
    let stored = read_watermark(pool, WATERMARK_KEY).await?;
    if is_unchanged(stored.as_deref(), &fingerprint, now, max_staleness_secs) {
        info!(
            max_staleness_secs,
            "memory-graph-refresh: corpus unchanged since last refresh; skipping nodes+edges refresh"
        );
        return Ok(RefreshOutcome::SkippedUnchanged);
    }

    // Run the two refreshes independently so a failure of one (e.g. a transient
    // lock) does not prevent the other. Both are cheap relative to the vectors HNSW.
    let nodes = queries::refresh_memory_unified_nodes(pool).await;
    let edges = queries::refresh_memory_unified_edges(pool).await;
    if let Err(e) = &nodes {
        error!(view = "memory_unified_nodes", error = %e, "matview refresh failed");
    }
    if let Err(e) = &edges {
        error!(view = "memory_unified_edges", error = %e, "matview refresh failed");
    }
    // Surface the first error; the per-view errors above already pinpoint which.
    nodes.and(edges)?;

    stats.memory_graph_refreshes.fetch_add(1, Ordering::Relaxed);
    // Stamp the watermark only after a fully successful refresh, so a partial
    // failure re-attempts next tick rather than being masked as "unchanged".
    write_watermark(pool, WATERMARK_KEY, &fingerprint, now).await?;
    info!("memory-graph-refresh: refreshed memory_unified_nodes + memory_unified_edges");
    Ok(RefreshOutcome::Refreshed)
}

/// Refresh the `memory_unified_node_vectors` matview + its HNSW — the expensive
/// semantic-search index (`memory_unified_search` seeds/joins from it) — on its
/// OWN cadence + data-change gate (separate watermark). Split from the structural
/// refresh above so the ~minutes-long HNSW re-maintenance over ~1M vectors runs on
/// a slower interval than the cheap nodes+edges refresh. Tolerates the matview
/// being absent (`42P01`) during the post-split transition — a soft skip (no
/// watermark stamp) so the refresh retries once the migration has built it.
pub async fn run_memory_vectors_refresh(
    pool: &PgPool,
    stats: &StatsTracker,
    max_staleness_secs: u64,
) -> Result<RefreshOutcome, sqlx::Error> {
    let (fingerprint, now) = corpus_fingerprint(pool).await?;
    let stored = read_watermark(pool, VECTORS_WATERMARK_KEY).await?;
    if is_unchanged(stored.as_deref(), &fingerprint, now, max_staleness_secs) {
        info!(
            max_staleness_secs,
            "memory-vectors-refresh: corpus unchanged since last refresh; skipping vectors refresh"
        );
        return Ok(RefreshOutcome::SkippedUnchanged);
    }

    match queries::refresh_memory_unified_node_vectors(pool).await {
        Ok(()) => {}
        Err(e) if queries::is_undefined_table(&e) => {
            info!(
                "memory_unified_node_vectors not present yet (post-split transition); skipping its refresh"
            );
            return Ok(RefreshOutcome::SkippedUnchanged);
        }
        Err(e) => {
            error!(view = "memory_unified_node_vectors", error = %e, "matview refresh failed");
            return Err(e);
        }
    }

    stats.memory_graph_refreshes.fetch_add(1, Ordering::Relaxed);
    write_watermark(pool, VECTORS_WATERMARK_KEY, &fingerprint, now).await?;
    info!("memory-vectors-refresh: refreshed memory_unified_node_vectors + HNSW");
    Ok(RefreshOutcome::Refreshed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_watermark_forces_refresh() {
        assert!(!is_unchanged(None, "10:20", 1_000, 86_400));
    }

    #[test]
    fn matching_fingerprint_within_window_skips() {
        // stored fp matches, refreshed 1h ago, 24h window → unchanged (skip).
        assert!(is_unchanged(Some("10:20|0"), "10:20", 3_600, 86_400));
    }

    #[test]
    fn changed_fingerprint_forces_refresh() {
        assert!(!is_unchanged(Some("10:20|0"), "11:21", 3_600, 86_400));
    }

    #[test]
    fn stale_watermark_forces_refresh() {
        // fp matches but last refresh is older than the window → refresh anyway.
        assert!(!is_unchanged(Some("10:20|0"), "10:20", 90_000, 86_400));
    }

    #[test]
    fn malformed_watermark_forces_refresh() {
        assert!(!is_unchanged(Some("garbage"), "10:20", 1_000, 86_400));
        assert!(!is_unchanged(
            Some("10:20|notanumber"),
            "10:20",
            1_000,
            86_400
        ));
    }
}
