//! Cross-language signature-clone materializer.
//!
//! Walks `file_symbols` rows that have populated `return_type_shape` +
//! `symbol_parameters`, computes a structural `signature_shape_hash`,
//! groups symbols by that hash, and inserts pairs spanning different
//! languages into `cross_language_signature_clones`.
//!
//! The cron runs periodically (default every 12h). Re-runs are
//! idempotent: the table is fully repopulated each pass since the
//! computation is O(N) per project pair and reproducing it is cheaper
//! than diffing. The pair table is empty when the shadow-ASR fields
//! aren't yet populated for any symbols.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use sqlx::PgPool;
use tracing::{debug, error, info};

use crate::mcp::tools::sema_helpers::signatures::signature_shape_hash;
use crate::stats::tracker::StatsTracker;

/// Scan + materialize cross-language signature clones for all projects.
/// Idempotent: rebuilds the table from scratch each pass.
pub async fn run_cross_language_signatures(
    pool: &PgPool,
    stats: &Arc<StatsTracker>,
) -> Result<u64, sqlx::Error> {
    let start = std::time::Instant::now();

    // Truncate + repopulate. The table is small enough (pair count grows
    // with within-shape clusters of differing-language symbols) that
    // full rebuilds are tractable.
    sqlx::query("TRUNCATE TABLE cross_language_signature_clones")
        .execute(pool)
        .await?;

    // Load all function-shaped symbols whose return_type_shape is set —
    // those are the ones with extractable shadow-ASR data. The query
    // intentionally avoids dropping symbols whose parameters table row
    // is empty: a no-arg function with a return type is still a valid
    // signature shape (e.g. property getters).
    let rows: Vec<(i64, i64, String, i32)> = sqlx::query_as(
        "SELECT fs.id, fs.file_id, f.language, f.project_id
         FROM file_symbols fs
         JOIN indexed_files f ON f.id = fs.file_id
         WHERE fs.kind = 'function'
           AND (fs.return_type_shape IS NOT NULL
                OR EXISTS (
                    SELECT 1 FROM symbol_parameters p WHERE p.symbol_id = fs.id
                ))",
    )
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        info!("cross_language_signatures: no candidate symbols");
        return Ok(0);
    }

    // Compute a structural hash per symbol. Group by hash → list of
    // (symbol_id, file_id, language, project_id).
    #[derive(Clone)]
    struct SymInfo {
        symbol_id: i64,
        language: String,
        project_id: i32,
    }
    let mut by_hash: HashMap<u64, Vec<SymInfo>> = HashMap::new();
    for (symbol_id, _file_id, language, project_id) in rows {
        // The signature descriptor fetch is one round-trip per symbol;
        // batchable later if this becomes hot. For now, prefer
        // correctness over throughput — the cron is bounded by 12h.
        let Some(sig) = crate::mcp::tools::sema_helpers::signatures::fetch_signature_descriptor(
            pool, symbol_id,
        )
        .await?
        else {
            continue;
        };
        let h = signature_shape_hash(&sig);
        by_hash.entry(h).or_default().push(SymInfo {
            symbol_id,
            language,
            project_id,
        });
    }

    // Emit pairs across different languages.
    let mut inserted_pairs: u64 = 0;
    for (hash, syms) in by_hash {
        if syms.len() < 2 {
            continue;
        }
        for i in 0..syms.len() {
            for j in (i + 1)..syms.len() {
                let a = &syms[i];
                let b = &syms[j];
                if a.language == b.language {
                    continue;
                }
                let (lower, upper) = if a.symbol_id < b.symbol_id {
                    (a, b)
                } else {
                    (b, a)
                };
                let res = sqlx::query(
                    "INSERT INTO cross_language_signature_clones (
                         symbol_id_a, symbol_id_b, signature_shape_hash,
                         similarity, language_a, language_b,
                         project_id_a, project_id_b
                     ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                     ON CONFLICT (symbol_id_a, symbol_id_b) DO NOTHING",
                )
                .bind(lower.symbol_id)
                .bind(upper.symbol_id)
                .bind(hash as i64)
                .bind(1.0_f32)
                .bind(&lower.language)
                .bind(&upper.language)
                .bind(lower.project_id)
                .bind(upper.project_id)
                .execute(pool)
                .await;
                match res {
                    Ok(r) => inserted_pairs += r.rows_affected(),
                    Err(e) => {
                        error!(error = ?e, "failed to insert cross-language clone pair");
                    }
                }
            }
        }
    }

    let elapsed_ms = start.elapsed().as_millis() as u64;
    info!(
        pairs = inserted_pairs,
        elapsed_ms, "cross_language_signatures materialization complete"
    );
    stats
        .cross_language_pairs_found
        .fetch_add(inserted_pairs, Ordering::Relaxed);

    Ok(inserted_pairs)
}

/// Convenience wrapper that accepts a `DbClient` instead of a raw
/// `PgPool` so it integrates with the existing cron dispatch path.
pub async fn run(
    db: &dyn crate::db::DbClient,
    stats: &Arc<StatsTracker>,
) -> Result<(), sqlx::Error> {
    let Some(pool) = db.pool() else {
        debug!("cross_language_signatures: skipping (no pool)");
        return Ok(());
    };
    run_cross_language_signatures(pool, stats).await?;
    Ok(())
}
