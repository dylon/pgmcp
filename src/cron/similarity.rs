//! Batch cross-project similarity scanner.
//!
//! Runs periodically to populate the `cross_project_similarities` materialized table.
//! For each chunk, uses `CROSS JOIN LATERAL ... ORDER BY embedding <=> ... LIMIT K`
//! to find top-K nearest neighbors from **other** projects via the HNSW index.
//! Only stores pairs above the configured similarity threshold.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tracing::{error, info, warn};

use crate::config::CronConfig;
use crate::cron::shutdown::{CronAction, classify_db_error};
use crate::daemon_state::DaemonLifecycle;
use crate::db::DbClient;
use crate::stats::tracker::StatsTracker;

/// Run a full batch similarity scan.
///
/// Iterates through all chunks in batches, finding cross-project nearest neighbors
/// for each chunk. Results are inserted into `cross_project_similarities`.
///
/// The table is truncated before each full scan to remove stale pairs from
/// deleted/re-indexed files.
pub async fn run_similarity_scan(
    db: &dyn DbClient,
    config: &CronConfig,
    ef_search: i32,
    stats: &Arc<StatsTracker>,
    lifecycle: &DaemonLifecycle,
) {
    let threshold = config.similarity_threshold;
    let top_k = config.similarity_top_k;
    let batch_size: i32 = 500;

    info!(
        threshold,
        top_k, batch_size, "Starting cross-project similarity scan"
    );

    // Promoted to top-of-body: this counter means "the body reached its
    // work-eligible state" — pairs with `similarity_noop_returns` to
    // distinguish "ran, no chunks" from "never ran".
    stats.similarity_scans.fetch_add(1, Ordering::Relaxed);

    // Truncate the table for a fresh scan
    if let Err(e) = db.clear_similarity_table().await {
        error!(error = %e, "Failed to clear similarity table");
        return;
    }

    let max_id = match db.max_chunk_id().await {
        Ok(id) => id,
        Err(e) => {
            error!(error = %e, "Failed to get max chunk ID");
            return;
        }
    };

    if max_id == 0 {
        stats
            .similarity_noop_returns
            .fetch_add(1, Ordering::Relaxed);
        info!("No chunks to scan for similarity");
        return;
    }

    let mut last_id: i64 = 0;
    let mut total_pairs: u64 = 0;
    let mut batches_processed: u64 = 0;

    loop {
        if last_id >= max_id {
            break;
        }

        if lifecycle.is_stopping() {
            info!(
                last_id,
                batches_processed, "similarity-scan: lifecycle stopping, breaking loop"
            );
            break;
        }

        let neighbors = match db
            .batch_find_cross_project_neighbors(last_id, batch_size, top_k, threshold, ef_search)
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                if classify_db_error(&e) == CronAction::AbortRun {
                    // Shutdown-time termination through the DB pool is expected
                    // behaviour, not a warning. The polite `is_stopping()` check
                    // at the top of the loop catches the cooperative path; this
                    // arm only fires when the pool closes mid-batch (i.e., the
                    // SIGTERM arrived between iterations). Either way: stop, no
                    // noise, one line in the log so the operator can correlate.
                    info!(
                        error = %e,
                        last_id,
                        batches_processed,
                        "similarity-scan: shutdown detected via pool error, exiting cleanly"
                    );
                    break;
                }
                error!(
                    error = %e,
                    last_id,
                    "Similarity batch query failed, skipping batch"
                );
                last_id += batch_size as i64;
                continue;
            }
        };

        // Update last_id to skip past this batch
        // If we got results, the max chunk_id_a tells us where we are.
        // If empty, advance by batch_size.
        let batch_max_a = neighbors.iter().map(|r| r.chunk_id_a).max();
        if let Some(max_a) = batch_max_a {
            last_id = max_a;
        } else {
            last_id += batch_size as i64;
        }

        if neighbors.is_empty() {
            batches_processed += 1;
            continue;
        }

        match db.insert_similarity_pairs(&neighbors).await {
            Ok(inserted) => {
                total_pairs += inserted;
            }
            Err(e) => {
                warn!(error = %e, "Failed to insert similarity batch");
            }
        }

        batches_processed += 1;

        if batches_processed.is_multiple_of(20) {
            info!(
                batches = batches_processed,
                pairs = total_pairs,
                progress_pct = (last_id as f64 / max_id as f64 * 100.0) as u32,
                "Similarity scan progress"
            );
        }
    }

    // `similarity_scans` was promoted to top-of-body above.
    stats
        .similarity_pairs_found
        .store(total_pairs, Ordering::Relaxed);

    info!(
        batches = batches_processed,
        pairs = total_pairs,
        "Cross-project similarity scan complete"
    );
}
