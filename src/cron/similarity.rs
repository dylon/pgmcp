//! Batch cross-project similarity scanner.
//!
//! Runs periodically to populate the `cross_project_similarities` materialized table.
//! For each chunk, uses `CROSS JOIN LATERAL ... ORDER BY embedding <=> ... LIMIT K`
//! to find top-K nearest neighbors from **other** projects via the HNSW index.
//! Only stores pairs above the configured similarity threshold.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use sqlx::PgPool;
use tracing::{error, info, warn};

use crate::config::CronConfig;
use crate::stats::tracker::StatsTracker;

/// Run a full batch similarity scan.
///
/// Iterates through all chunks in batches, finding cross-project nearest neighbors
/// for each chunk. Results are inserted into `cross_project_similarities`.
///
/// The table is truncated before each full scan to remove stale pairs from
/// deleted/re-indexed files.
pub async fn run_similarity_scan(
    pool: &PgPool,
    config: &CronConfig,
    ef_search: i32,
    stats: &Arc<StatsTracker>,
) {
    let threshold = config.similarity_threshold;
    let top_k = config.similarity_top_k;
    let batch_size: i32 = 500;

    info!(
        threshold,
        top_k, batch_size, "Starting cross-project similarity scan"
    );

    // Truncate the table for a fresh scan
    if let Err(e) = crate::db::queries::clear_similarity_table(pool).await {
        error!(error = %e, "Failed to clear similarity table");
        return;
    }

    let max_id = match crate::db::queries::max_chunk_id(pool).await {
        Ok(id) => id,
        Err(e) => {
            error!(error = %e, "Failed to get max chunk ID");
            return;
        }
    };

    if max_id == 0 {
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

        let neighbors = match crate::db::queries::batch_find_cross_project_neighbors(
            pool, last_id, batch_size, top_k, threshold, ef_search,
        )
        .await
        {
            Ok(rows) => rows,
            Err(e) => {
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

        match crate::db::queries::insert_similarity_pairs(pool, &neighbors).await {
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

    stats.similarity_scans.fetch_add(1, Ordering::Relaxed);
    stats
        .similarity_pairs_found
        .store(total_pairs, Ordering::Relaxed);

    info!(
        batches = batches_processed,
        pairs = total_pairs,
        "Cross-project similarity scan complete"
    );
}
