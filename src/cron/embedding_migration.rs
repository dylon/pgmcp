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
use crate::db::queries;
use crate::embed::admission;
use crate::embed::model::{BGE_M3_SIGNATURE, Embedder};
use crate::indexer::contextualize::{ChunkContext, build_context_prefix};
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

/// Outcome of one cron pass. Phase 5 C5: extended with four new
/// counters covering the additional tables that the full BGE-M3
/// migration drains beyond Phase 1's file_chunks + session_prompts.
#[derive(Debug, Default)]
pub struct MigrationPassReport {
    pub file_chunks_migrated: u64,
    pub session_prompts_migrated: u64,
    pub git_commit_chunks_migrated: u64,
    pub software_pattern_chunks_migrated: u64,
    pub durable_mandates_migrated: u64,
    pub session_mandates_migrated: u64,
    /// Phase 2.3: file_chunks whose BGE-M3 learned-sparse vector was backfilled.
    pub file_chunks_sparse_backfilled: u64,
    /// Phase 2.4: file_chunks re-embedded with a contextual-retrieval prefix.
    pub file_chunks_contextualized: u64,
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

    // Skip the whole pass — including the GPU model upload — only when every
    // backfill this pass performs has drained: the dense backlog
    // (embedding_v2/embedding NULL) AND the contextual re-embed backlog
    // (file_chunks.contextual_text NULL with a dense vector present). Counting
    // the dense backlog alone stranded the contextual leg the instant cutover
    // completed — it runs later in the pass (below), so once dense hit 0 this
    // guard returned every tick and contextual_text froze partway. Sparse is
    // intentionally excluded here: it is gated on `embedder.has_sparse()`
    // (false unless `sparse_linear.pt` is wired into the checkpoint load), so
    // including it would block the short-circuit forever on a dense-only
    // checkpoint and rebuild the embedder for a no-op every tick.
    let dense_backlog = full_backlog_counts(pool).await?.total();
    let contextual_backlog = contextual_backlog_count(pool).await?;
    if dense_backlog == 0 && contextual_backlog == 0 {
        debug!("embedding-migration: dense + contextual backlog empty; skipping pass");
        return Ok(MigrationPassReport::default());
    }

    // GPU admission: take a resident-copy permit before constructing the
    // embedder so the migration's copy plus the always-on pool workers can't
    // exceed `embeddings.gpu_max_resident_embedders`. Non-blocking — if no slot
    // is free (the pool is using the whole budget) defer to the next tick
    // rather than piling a third BGE-M3 onto a full GPU. Held for the entire
    // pass; released when the embedder is dropped at function exit.
    let _gpu_permit = match admission::try_acquire_owned() {
        admission::Admission::Disabled => None, // CPU mode → proceed unguarded
        admission::Admission::Granted(permit) => Some(permit),
        admission::Admission::Deferred => {
            info!("embedding-migration: GPU embedder budget exhausted; deferring to next tick");
            return Ok(MigrationPassReport::default());
        }
    };

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

    // Phase 2.3: backfill BGE-M3 learned-sparse vectors for already-dense
    // file_chunks. Only when the embedder exposes a sparse head; purely
    // additive — never touches `embedding_v2`, and downstream search is
    // NULL-tolerant, so dense + BM25 retrieval is unaffected if this lags.
    if embedder.has_sparse() {
        for _ in 0..config.max_batches {
            match backfill_file_chunks_sparse_batch(pool, &embedder, config.batch_size).await {
                Ok(n) if n > 0 => {
                    report.file_chunks_sparse_backfilled += n;
                    report.batches_completed += 1;
                }
                Ok(_) => break,
                Err(e) => {
                    warn!(error = %e, "file_chunks sparse backfill batch failed");
                    report.errors += 1;
                    stats
                        .embeddings_migration_errors
                        .fetch_add(1, Ordering::Relaxed);
                    break;
                }
            }
        }
    }

    // Phase 2.4: contextual re-embed — prepend a deterministic situating prefix
    // (file/symbol/importers) and re-embed `embedding_v2` from prefix||content.
    // Dense leg only (primary semantic signal); NULL-tolerant — un-processed
    // chunks keep their non-contextual embedding, comparable since same
    // model/dim. Runs after symbol-extraction/graph-analysis ideally, but
    // degrades gracefully (thinner prefix) if those lag.
    for _ in 0..config.max_batches {
        match contextualize_file_chunks_batch(pool, &embedder, config.batch_size).await {
            Ok(n) if n > 0 => {
                report.file_chunks_contextualized += n;
                report.batches_completed += 1;
            }
            Ok(_) => break,
            Err(e) => {
                warn!(error = %e, "file_chunks contextual re-embed batch failed");
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

    // Phase 5 C5: drain the four additional tables that the Phase 1
    // milestone didn't cover. Each follows the same SKIP LOCKED batch
    // pattern as file_chunks / session_prompts so multiple daemon
    // instances or a manual `pgmcp` CLI invocation can run concurrently
    // without double-embedding.
    for _ in 0..config.max_batches {
        match migrate_git_commit_chunks_batch(pool, &embedder, config.batch_size).await {
            Ok(n) if n > 0 => {
                report.git_commit_chunks_migrated += n;
                report.batches_completed += 1;
            }
            Ok(_) => break,
            Err(e) => {
                warn!(error = %e, "git_commit_chunks migration batch failed");
                report.errors += 1;
                stats
                    .embeddings_migration_errors
                    .fetch_add(1, Ordering::Relaxed);
                break;
            }
        }
    }
    for _ in 0..config.max_batches {
        match migrate_software_pattern_chunks_batch(pool, &embedder, config.batch_size).await {
            Ok(n) if n > 0 => {
                report.software_pattern_chunks_migrated += n;
                report.batches_completed += 1;
            }
            Ok(_) => break,
            Err(e) => {
                warn!(error = %e, "software_pattern_chunks migration batch failed");
                report.errors += 1;
                stats
                    .embeddings_migration_errors
                    .fetch_add(1, Ordering::Relaxed);
                break;
            }
        }
    }
    for _ in 0..config.max_batches {
        match migrate_durable_mandates_batch(pool, &embedder, config.batch_size).await {
            Ok(n) if n > 0 => {
                report.durable_mandates_migrated += n;
                report.batches_completed += 1;
            }
            Ok(_) => break,
            Err(e) => {
                warn!(error = %e, "durable_mandates migration batch failed");
                report.errors += 1;
                stats
                    .embeddings_migration_errors
                    .fetch_add(1, Ordering::Relaxed);
                break;
            }
        }
    }
    for _ in 0..config.max_batches {
        match migrate_session_mandates_batch(pool, &embedder, config.batch_size).await {
            Ok(n) if n > 0 => {
                report.session_mandates_migrated += n;
                report.batches_completed += 1;
            }
            Ok(_) => break,
            Err(e) => {
                warn!(error = %e, "session_mandates migration batch failed");
                report.errors += 1;
                stats
                    .embeddings_migration_errors
                    .fetch_add(1, Ordering::Relaxed);
                break;
            }
        }
    }

    if report.file_chunks_migrated > 0
        || report.session_prompts_migrated > 0
        || report.git_commit_chunks_migrated > 0
        || report.software_pattern_chunks_migrated > 0
        || report.durable_mandates_migrated > 0
        || report.session_mandates_migrated > 0
    {
        info!(
            file_chunks = report.file_chunks_migrated,
            session_prompts = report.session_prompts_migrated,
            git_commit_chunks = report.git_commit_chunks_migrated,
            software_pattern_chunks = report.software_pattern_chunks_migrated,
            durable_mandates = report.durable_mandates_migrated,
            session_mandates = report.session_mandates_migrated,
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

/// Drain up to `batch_size` `file_chunks` rows that have a dense `embedding_v2`
/// but no `sparse_v2`, computing the BGE-M3 learned-sparse vector for each
/// (graph-roadmap Phase 2.3). Returns the number backfilled (0 = drained).
/// Every BGE-M3 chunk gets a vector (empty when no salient tokens) so it is not
/// re-scanned. `FOR UPDATE SKIP LOCKED` makes concurrent passes safe.
async fn backfill_file_chunks_sparse_batch(
    pool: &PgPool,
    embedder: &Arc<Embedder>,
    batch_size: usize,
) -> Result<u64, sqlx::Error> {
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, content FROM file_chunks
         WHERE sparse_v2 IS NULL AND embedding_v2 IS NOT NULL
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
    let sparses = match embedder.embed_sparse(&texts) {
        Ok(v) => v,
        Err(e) => return Err(sqlx::Error::Configuration(e.to_string().into())),
    };
    if sparses.len() != rows.len() {
        return Err(sqlx::Error::Protocol(format!(
            "embed_sparse returned {} vectors for {} inputs",
            sparses.len(),
            rows.len()
        )));
    }
    let mut tx = pool.begin().await?;
    let mut count = 0_u64;
    for ((id, _), sparse) in rows.into_iter().zip(sparses) {
        let Some(sv) = sparse else { continue };
        sqlx::query("UPDATE file_chunks SET sparse_v2 = $1 WHERE id = $2")
            .bind(&sv)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        count += 1;
    }
    tx.commit().await?;
    Ok(count)
}

/// Drain up to `batch_size` `file_chunks` with a dense `embedding_v2` but no
/// `contextual_text`, prepend the deterministic contextual prefix, and
/// re-embed `embedding_v2` from `prefix || content` (graph-roadmap Phase 2.4).
/// Stamps `contextual_text` (empty string when no context) so the row is not
/// re-processed. Returns the number contextualized (0 = drained).
async fn contextualize_file_chunks_batch(
    pool: &PgPool,
    embedder: &Arc<Embedder>,
    batch_size: usize,
) -> Result<u64, sqlx::Error> {
    let rows = queries::get_chunks_needing_context(pool, batch_size as i32).await?;
    if rows.is_empty() {
        return Ok(0);
    }
    let mut prefixes: Vec<String> = Vec::with_capacity(rows.len());
    let mut prefixed: Vec<String> = Vec::with_capacity(rows.len());
    for r in &rows {
        let ctx = ChunkContext {
            relative_path: r.relative_path.clone(),
            language: r.language.clone(),
            symbol_kind: r.symbol_kind.clone(),
            symbol_name: r.symbol_name.clone(),
            symbol_signature: r.symbol_signature.clone(),
            topics: Vec::new(),
            importer_count: r.importer_count,
        };
        let prefix = build_context_prefix(&ctx);
        prefixed.push(format!("{prefix}{}", r.content));
        prefixes.push(prefix);
    }
    let texts: Vec<&str> = prefixed.iter().map(|s| s.as_str()).collect();
    let vectors = match embedder.embed(&texts) {
        Ok(v) => v,
        Err(e) => return Err(sqlx::Error::Configuration(e.to_string().into())),
    };
    if vectors.len() != rows.len() {
        return Err(sqlx::Error::Protocol(format!(
            "embed returned {} vectors for {} inputs",
            vectors.len(),
            rows.len()
        )));
    }
    let mut tx = pool.begin().await?;
    let mut count = 0_u64;
    for ((r, prefix), vec) in rows.iter().zip(prefixes).zip(vectors) {
        let v = Vector::from(vec);
        sqlx::query(
            "UPDATE file_chunks
             SET embedding_v2 = $1, contextual_text = $2, embedding_signature = $3
             WHERE id = $4",
        )
        .bind(&v)
        .bind(prefix.as_str())
        .bind(BGE_M3_SIGNATURE)
        .bind(r.id)
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

/// Phase 5 C5: drain `git_commit_chunks`. Writes 1024d BGE-M3
/// embeddings into the new `embedding_v2` column with
/// `embedding_signature = 'bge-m3-v1'`.
async fn migrate_git_commit_chunks_batch(
    pool: &PgPool,
    embedder: &Arc<Embedder>,
    batch_size: usize,
) -> Result<u64, sqlx::Error> {
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, content FROM git_commit_chunks
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
            "UPDATE git_commit_chunks
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

/// Phase 5 C5: drain `software_pattern_chunks` (~600 rows; usually
/// finishes in a single tick).
async fn migrate_software_pattern_chunks_batch(
    pool: &PgPool,
    embedder: &Arc<Embedder>,
    batch_size: usize,
) -> Result<u64, sqlx::Error> {
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, content FROM software_pattern_chunks
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
            "UPDATE software_pattern_chunks
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

/// Phase 5 C5: populate `durable_mandates.embedding` (already 1024d-
/// shaped per the Phase 1 schema; no `_v2` column needed). The mandate
/// tables were created with the target shape directly but never had a
/// writer; this batch helper closes that gap.
async fn migrate_durable_mandates_batch(
    pool: &PgPool,
    embedder: &Arc<Embedder>,
    batch_size: usize,
) -> Result<u64, sqlx::Error> {
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, imperative FROM durable_mandates
         WHERE embedding IS NULL
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
            "UPDATE durable_mandates
             SET embedding = $1, embedding_signature = $2
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

/// Phase 5 C5: populate `session_mandates.embedding`. Same shape as
/// `migrate_durable_mandates_batch`. The session mandate dedupe in
/// `sessions::mark_near_duplicate_superseded` does NOT consume this
/// embedding (it runs the in-process DynamicDawgChar dedup from
/// Phase 3), but the memory-server reranker and PPR helpers do once
/// `pgmcp embed-cutover --to bge-m3` flips the active signature.
async fn migrate_session_mandates_batch(
    pool: &PgPool,
    embedder: &Arc<Embedder>,
    batch_size: usize,
) -> Result<u64, sqlx::Error> {
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, imperative FROM session_mandates
         WHERE embedding IS NULL
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
            "UPDATE session_mandates
             SET embedding = $1, embedding_signature = $2
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

/// Returns true once every BGE-M3-migration-bearing table is fully
/// drained (zero rows with `embedding_v2 IS NULL`, or for the mandate
/// tables which were authored 1024d-direct, zero rows with
/// `embedding IS NULL`).
///
/// Phase 5 C5 extends the Phase 1 check from 2 tables to the full 6:
/// file_chunks, session_prompts, git_commit_chunks,
/// software_pattern_chunks, durable_mandates, session_mandates.
/// Operators use this before flipping
/// `pgmcp_metadata.active_embedding_signature` to `bge-m3-v1` — flipping
/// before the drain leaves cold rows that hash against the wrong column.
///
/// Cheap to call: counts NULLs over partial indices.
pub async fn migration_complete(pool: &PgPool) -> Result<bool, sqlx::Error> {
    let counts = full_backlog_counts(pool).await?;
    Ok(counts.total() == 0)
}

/// Per-table backlog counts. Used by `pgmcp embed-cutover --check`
/// (lands in C9) to give the operator a row-level picture before the
/// flip; `migration_complete` is the cheap boolean wrapper around
/// `total() == 0`.
#[derive(Debug, Default, Clone, Copy)]
pub struct BacklogCounts {
    pub file_chunks: i64,
    pub session_prompts: i64,
    pub git_commit_chunks: i64,
    pub software_pattern_chunks: i64,
    pub durable_mandates: i64,
    pub session_mandates: i64,
}

impl BacklogCounts {
    pub fn total(&self) -> i64 {
        self.file_chunks
            + self.session_prompts
            + self.git_commit_chunks
            + self.software_pattern_chunks
            + self.durable_mandates
            + self.session_mandates
    }
}

/// Read the per-table backlog. One round trip via UNION ALL of six
/// COUNT(*) probes.
pub async fn full_backlog_counts(pool: &PgPool) -> Result<BacklogCounts, sqlx::Error> {
    let row: (i64, i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT COUNT(*) FROM file_chunks            WHERE embedding_v2 IS NULL),
            (SELECT COUNT(*) FROM session_prompts        WHERE embedding_v2 IS NULL),
            (SELECT COUNT(*) FROM git_commit_chunks      WHERE embedding_v2 IS NULL),
            (SELECT COUNT(*) FROM software_pattern_chunks WHERE embedding_v2 IS NULL),
            (SELECT COUNT(*) FROM durable_mandates       WHERE embedding    IS NULL),
            (SELECT COUNT(*) FROM session_mandates       WHERE embedding    IS NULL)",
    )
    .fetch_one(pool)
    .await?;
    Ok(BacklogCounts {
        file_chunks: row.0,
        session_prompts: row.1,
        git_commit_chunks: row.2,
        software_pattern_chunks: row.3,
        durable_mandates: row.4,
        session_mandates: row.5,
    })
}

/// Count `file_chunks` still needing a contextual re-embed (graph-roadmap
/// Phase 2.4). This MUST mirror the exact selectable set of
/// `queries::get_chunks_needing_context` — the INNER `JOIN indexed_files`,
/// `contextual_text IS NULL`, and `embedding_v2 IS NOT NULL` — so the
/// short-circuit in `run_embedding_migration_pass` never diverges from what the
/// drain can actually process. If this counted rows the drain can't reach (e.g.
/// chunks orphaned from `indexed_files`), the pass would spin forever rebuilding
/// the embedder; if it counted fewer, contextual would be stranded again.
///
/// Distinct from `full_backlog_counts` (dense embeddings): contextual is an
/// additive re-embed of already-dense rows, so it is tracked separately and the
/// guard ORs the two backlogs.
async fn contextual_backlog_count(pool: &PgPool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM file_chunks c
         JOIN indexed_files f ON f.id = c.file_id
         WHERE c.contextual_text IS NULL AND c.embedding_v2 IS NOT NULL",
    )
    .fetch_one(pool)
    .await
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
