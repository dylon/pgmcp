//! Memory-server Phase 1: BGE-M3 embedding migration cron.
//!
//! Drains `file_chunks` and `session_prompts` rows whose `embedding_v2`
//! column is NULL, embeds the source text with the BGE-M3 backbone, and
//! writes back `embedding_v2` + `embedding_signature = 'bge-m3-v1'`.
//!
//! See `docs/memory-server/02-phases.md` Phase 1 for the broader migration
//! design (parallel columns, manual cutover via
//! `pgmcp_metadata.active_embedding_signature`). This module owns the
//! background-fill half — the read-side cutover is handled in
//! `src/db/queries.rs`.
//!
//! The cron is **off by default** in the daemon's cron registry; the
//! operator enables it once they're ready to begin migration. While
//! enabled, it polls on a configurable interval (default 10 minutes per
//! `EmbeddingMigrationConfig`).
//!
//! Each pass embeds at most `batch_size × max_batches` rows so a single
//! tick can't hold the GPU for hours; the next tick picks up where the
//! prior left off (no per-row state — `WHERE embedding_v2 IS NULL` is the
//! cursor).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use pgvector::Vector;
use sqlx::PgPool;
use tracing::{debug, error, info, warn};

use crate::config::EmbeddingsConfig;
use crate::embed::model::{BGE_M3_SIGNATURE, Embedder};
use crate::stats::tracker::StatsTracker;

/// Configuration for the embedding-migration cron pass. Built from the
/// `[memory.embedder]` and `[cron]` config sections by the daemon.
#[derive(Debug, Clone)]
pub struct EmbeddingMigrationConfig {
    /// Embeddings config used to construct the BGE-M3 backbone. Must have
    /// `model = "bge-m3"`.
    pub embeddings: EmbeddingsConfig,
    /// Rows embedded in one forward pass through the model. Default 64;
    /// XLM-RoBERTa-Large self-attention is O(batch · seq²) and 64×512
    /// fits well under the 1.5 GB-ish activation budget on an RTX 4060 Ti.
    pub batch_size: usize,
    /// Cap on the number of batches processed per cron tick across both
    /// tables. Prevents one tick from monopolizing the GPU when the
    /// backlog is large. Default 32 (= 32 × 64 = 2048 rows / tick).
    pub max_batches: usize,
}

impl EmbeddingMigrationConfig {
    pub fn new(mut embeddings: EmbeddingsConfig, batch_size: usize, max_batches: usize) -> Self {
        // The migration cron is BGE-M3-specific. Force the model field so
        // a stale `[memory.embedder] backend = "minilm"` config doesn't
        // accidentally generate 384d vectors here.
        embeddings.model = "bge-m3".into();
        embeddings.dimensions = 1024;
        Self {
            embeddings,
            batch_size: if batch_size == 0 { 64 } else { batch_size },
            max_batches: if max_batches == 0 { 32 } else { max_batches },
        }
    }
}

/// Outcome of one cron pass.
#[derive(Debug, Default)]
pub struct MigrationPassReport {
    pub file_chunks_migrated: u64,
    pub session_prompts_migrated: u64,
    pub batches_completed: u64,
    /// Number of batches that errored (and are retried on the next tick
    /// because the WHERE clause picks them back up).
    pub errors: u64,
}

/// Run one migration pass. Returns the per-pass report; the caller
/// (`run_or_log`) is responsible for translating into telemetry.
///
/// The function builds an `Embedder` per call. Subsequent calls hit the
/// HF cache and the mmap is essentially free; the actual model upload
/// to GPU happens once per tick.
pub async fn run_embedding_migration_pass(
    pool: &PgPool,
    stats: &StatsTracker,
    config: &EmbeddingMigrationConfig,
) -> Result<MigrationPassReport, sqlx::Error> {
    stats
        .embeddings_migration_runs
        .fetch_add(1, Ordering::Relaxed);

    // Construct the embedder. If model load fails we surface the error so
    // the caller can log; failing here doesn't loop forever because the
    // cron scheduler retries on the next tick.
    let embedder = match Embedder::new(&config.embeddings) {
        Ok(e) => Arc::new(e),
        Err(e) => {
            error!(error = %e, "embedding-migration: failed to construct BGE-M3 embedder");
            stats
                .embeddings_migration_errors
                .fetch_add(1, Ordering::Relaxed);
            // Bubble up as a generic Postgres error type so the caller's
            // signature stays uniform — wrap into a `Database` error.
            return Err(sqlx::Error::Configuration(e.to_string().into()));
        }
    };

    let mut report = MigrationPassReport::default();

    for _ in 0..config.max_batches {
        let drained = migrate_file_chunks_batch(pool, &embedder, config.batch_size).await;
        match drained {
            Ok(n) if n > 0 => {
                report.file_chunks_migrated += n;
                report.batches_completed += 1;
                stats
                    .embeddings_migrated_file_chunks
                    .fetch_add(n, Ordering::Relaxed);
            }
            Ok(_) => break, // backlog drained for file_chunks; move on
            Err(e) => {
                warn!(error = %e, "file_chunks migration batch failed");
                report.errors += 1;
                stats
                    .embeddings_migration_errors
                    .fetch_add(1, Ordering::Relaxed);
                break;
            }
        }
    }

    for _ in 0..config.max_batches {
        let drained = migrate_session_prompts_batch(pool, &embedder, config.batch_size).await;
        match drained {
            Ok(n) if n > 0 => {
                report.session_prompts_migrated += n;
                report.batches_completed += 1;
                stats
                    .embeddings_migrated_session_prompts
                    .fetch_add(n, Ordering::Relaxed);
            }
            Ok(_) => break,
            Err(e) => {
                warn!(error = %e, "session_prompts migration batch failed");
                report.errors += 1;
                stats
                    .embeddings_migration_errors
                    .fetch_add(1, Ordering::Relaxed);
                break;
            }
        }
    }

    if report.file_chunks_migrated > 0 || report.session_prompts_migrated > 0 {
        info!(
            file_chunks = report.file_chunks_migrated,
            session_prompts = report.session_prompts_migrated,
            batches = report.batches_completed,
            errors = report.errors,
            "embedding-migration pass complete",
        );
    } else {
        debug!(
            errors = report.errors,
            "embedding-migration: no backlog remaining"
        );
    }

    Ok(report)
}

/// Drain up to `batch_size` `file_chunks` rows. Returns the number of
/// rows whose `embedding_v2` was populated by this call (0 = backlog
/// empty).
async fn migrate_file_chunks_batch(
    pool: &PgPool,
    embedder: &Arc<Embedder>,
    batch_size: usize,
) -> Result<u64, sqlx::Error> {
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, content FROM file_chunks
         WHERE embedding_v2 IS NULL
         ORDER BY id
         LIMIT $1
         FOR UPDATE SKIP LOCKED",
    )
    .bind(batch_size as i64)
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        return Ok(0);
    }

    let texts: Vec<&str> = rows.iter().map(|(_, c)| c.as_str()).collect();
    let vectors = match embedder.embed(&texts) {
        Ok(v) => v,
        Err(e) => return Err(sqlx::Error::Configuration(e.to_string().into())),
    };
    if vectors.len() != rows.len() {
        return Err(sqlx::Error::Protocol(format!(
            "embedder returned {} vectors for {} inputs",
            vectors.len(),
            rows.len()
        )));
    }

    let mut tx = pool.begin().await?;
    let mut count = 0_u64;
    for ((id, _), vec) in rows.into_iter().zip(vectors) {
        let v = Vector::from(vec);
        sqlx::query(
            "UPDATE file_chunks
             SET embedding_v2 = $1, embedding_signature = $2
             WHERE id = $3",
        )
        .bind(&v)
        .bind(BGE_M3_SIGNATURE)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        count += 1;
    }
    tx.commit().await?;
    Ok(count)
}

/// Drain up to `batch_size` `session_prompts` rows. Returns the number of
/// rows migrated.
async fn migrate_session_prompts_batch(
    pool: &PgPool,
    embedder: &Arc<Embedder>,
    batch_size: usize,
) -> Result<u64, sqlx::Error> {
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, prompt_text FROM session_prompts
         WHERE embedding_v2 IS NULL
         ORDER BY id
         LIMIT $1
         FOR UPDATE SKIP LOCKED",
    )
    .bind(batch_size as i64)
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        return Ok(0);
    }

    let texts: Vec<&str> = rows.iter().map(|(_, c)| c.as_str()).collect();
    let vectors = match embedder.embed(&texts) {
        Ok(v) => v,
        Err(e) => return Err(sqlx::Error::Configuration(e.to_string().into())),
    };

    let mut tx = pool.begin().await?;
    let mut count = 0_u64;
    for ((id, _), vec) in rows.into_iter().zip(vectors) {
        let v = Vector::from(vec);
        sqlx::query(
            "UPDATE session_prompts
             SET embedding_v2 = $1, embedding_signature = $2
             WHERE id = $3",
        )
        .bind(&v)
        .bind(BGE_M3_SIGNATURE)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        count += 1;
    }
    tx.commit().await?;
    Ok(count)
}

/// Daemon-facing entry point. Logs and swallows errors so a single bad
/// tick doesn't kill the cron thread.
pub async fn run_or_log(
    pool: Arc<PgPool>,
    stats: Arc<StatsTracker>,
    config: EmbeddingMigrationConfig,
) {
    if let Err(e) = run_embedding_migration_pass(&pool, &stats, &config).await {
        warn!(error = %e, "embedding-migration pass failed");
    }
}

// ============================================================================
// Operator helpers
// ============================================================================

/// Returns true once both `file_chunks` and `session_prompts` are fully
/// migrated (zero rows with `embedding_v2 IS NULL`). Operators use this
/// before flipping `pgmcp_metadata.active_embedding_signature` to
/// `bge-m3-v1` — flipping before the drain leaves cold rows that hash
/// against the wrong column.
///
/// Cheap to call: counts NULLs over partial indices.
pub async fn migration_complete(pool: &PgPool) -> Result<bool, sqlx::Error> {
    let pending: (i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT COUNT(*) FROM file_chunks WHERE embedding_v2 IS NULL),
            (SELECT COUNT(*) FROM session_prompts WHERE embedding_v2 IS NULL)",
    )
    .fetch_one(pool)
    .await?;
    Ok(pending.0 == 0 && pending.1 == 0)
}

/// Flip the cutover flag in `pgmcp_metadata`. Validates `migration_complete`
/// first to refuse a flip while backlog remains. The caller can override
/// the safety check with `force = true` (e.g. for tests or recoveries).
pub async fn promote_to_bge_m3(pool: &PgPool, force: bool) -> Result<(), sqlx::Error> {
    if !force && !migration_complete(pool).await? {
        return Err(sqlx::Error::Configuration(
            "embedding migration incomplete — rows still have embedding_v2 IS NULL. \
             Pass force=true to override (not recommended)."
                .into(),
        ));
    }
    sqlx::query(
        "INSERT INTO pgmcp_metadata (key, value)
         VALUES ('active_embedding_signature', $1)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(BGE_M3_SIGNATURE)
    .execute(pool)
    .await?;
    Ok(())
}

/// Read the current active embedding signature. Returns "minilm-l6-v2"
/// when the row hasn't been written yet, mirroring the migration default.
pub async fn active_embedding_signature(pool: &PgPool) -> Result<String, sqlx::Error> {
    let sig: Option<String> = sqlx::query_scalar(
        "SELECT value FROM pgmcp_metadata WHERE key = 'active_embedding_signature'",
    )
    .fetch_optional(pool)
    .await?;
    Ok(sig.unwrap_or_else(|| "minilm-l6-v2".into()))
}
