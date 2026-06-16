//! Chunk-level queries (insert/context-backfill/region reads/bulk
//! embedding extraction/per-file summaries/rerank features). Extracted from `queries.rs` (god-file split).
#![allow(unused_imports)]

use crate::db::queries::*;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

/// Insert a chunk with its embedding.
pub async fn insert_chunk(
    pool: &PgPool,
    file_id: i64,
    chunk_index: i32,
    content: &str,
    start_line: i32,
    end_line: i32,
    embedding: &[f32],
) -> Result<(), sqlx::Error> {
    // BGE-M3-only: chunks are written to the 1024-d `embedding_v2`
    // column with the `bge-m3-v1` signature. Any other dim is a
    // configuration error (model and DB out of sync) — refuse rather
    // than silently misroute.
    if embedding.len() != 1024 {
        return Err(sqlx::Error::Protocol(format!(
            "insert_chunk: expected a 1024-dimension BGE-M3 embedding, got {}",
            embedding.len()
        )));
    }
    let embedding_vec = pgvector::Vector::from(embedding.to_vec());
    sqlx::query(
        "INSERT INTO file_chunks
            (file_id, chunk_index, content, start_line, end_line,
             embedding_v2, embedding_signature)
         VALUES ($1, $2, $3, $4, $5, $6, 'bge-m3-v1')
         ON CONFLICT (file_id, chunk_index) DO UPDATE SET
            content = EXCLUDED.content,
            start_line = EXCLUDED.start_line,
            end_line = EXCLUDED.end_line,
            embedding_v2 = EXCLUDED.embedding_v2,
            embedding_signature = EXCLUDED.embedding_signature",
    )
    .bind(file_id)
    .bind(chunk_index)
    .bind(content)
    .bind(start_line)
    .bind(end_line)
    .bind(embedding_vec)
    .execute(pool)
    .await?;
    Ok(())
}

/// Borrowed chunk row for batch insertion.
///
/// The embed-pool worker builds these per-chunk references after the
/// model forward pass; the lifetime tying everything to the surrounding
/// `chunks: &[ChunkData]` and `embeddings: &[Vec<f32>]` allocations
/// keeps the per-batch payload heap-free.
pub struct ChunkInsert<'a> {
    pub chunk_index: i32,
    pub content: &'a str,
    pub start_line: i32,
    pub end_line: i32,
    pub embedding: &'a [f32],
}

/// Complete replacement payload for one indexed file.
///
/// `content_hash` is the final hash for the replacement. The transaction writes
/// `indexed_files.content_hash = NULL` first, replaces chunks, then finalizes the
/// hash before commit, so a lock timeout or insert error rolls the file back to
/// its previous complete state.
#[derive(Clone)]
pub struct IndexedFileReplacement<'a> {
    pub project_id: i32,
    pub path: &'a str,
    pub relative_path: &'a str,
    pub language: &'a str,
    pub size_bytes: i64,
    pub content: Option<&'a str>,
    pub content_hash: i64,
    pub line_count: i32,
    pub truncated: bool,
    pub content_recoverable_from_disk: bool,
    pub modified_at: DateTime<Utc>,
    pub chunks: &'a [ChunkInsert<'a>],
}

/// Atomically replace an indexed file's metadata and chunks.
///
/// This is the active indexer's all-or-nothing write path. It deliberately does
/// not reuse [`insert_chunks_batch`] because that helper assumes the caller has
/// already mutated `indexed_files`; here the metadata update, chunk delete,
/// chunk inserts, and content-hash finalization must live in one transaction.
pub async fn replace_indexed_file(
    pool: &PgPool,
    replacement: IndexedFileReplacement<'_>,
) -> Result<i64, sqlx::Error> {
    for chunk in replacement.chunks {
        if chunk.embedding.len() != 1024 {
            return Err(sqlx::Error::Protocol(format!(
                "replace_indexed_file: expected a 1024-dimension BGE-M3 \
                 embedding, got {}",
                chunk.embedding.len()
            )));
        }
    }

    let mut tx = pool.begin().await?;
    let res = async {
        let file_id = sqlx::query_scalar::<_, i64>(
            "INSERT INTO indexed_files
                (project_id, path, relative_path, language, size_bytes, content,
                 content_hash, line_count, truncated, content_recoverable_from_disk,
                 modified_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
             ON CONFLICT (path) DO UPDATE SET
                project_id = EXCLUDED.project_id,
                relative_path = EXCLUDED.relative_path,
                language = EXCLUDED.language,
                size_bytes = EXCLUDED.size_bytes,
                content = EXCLUDED.content,
                content_hash = EXCLUDED.content_hash,
                line_count = EXCLUDED.line_count,
                truncated = EXCLUDED.truncated,
                content_recoverable_from_disk = EXCLUDED.content_recoverable_from_disk,
                modified_at = EXCLUDED.modified_at,
                indexed_at = NOW()
             RETURNING id",
        )
        .bind(replacement.project_id)
        .bind(replacement.path)
        .bind(replacement.relative_path)
        .bind(replacement.language)
        .bind(replacement.size_bytes)
        .bind(replacement.content)
        .bind(Option::<i64>::None)
        .bind(replacement.line_count)
        .bind(replacement.truncated)
        .bind(replacement.content_recoverable_from_disk)
        .bind(replacement.modified_at)
        .fetch_one(&mut *tx)
        .await?;

        sqlx::query("DELETE FROM file_chunks WHERE file_id = $1")
            .bind(file_id)
            .execute(&mut *tx)
            .await?;

        for chunk in replacement.chunks {
            let embedding_vec = pgvector::Vector::from(chunk.embedding.to_vec());
            sqlx::query(
                "INSERT INTO file_chunks
                    (file_id, chunk_index, content, start_line, end_line,
                     embedding_v2, embedding_signature)
                 VALUES ($1, $2, $3, $4, $5, $6, 'bge-m3-v1')
                 ON CONFLICT (file_id, chunk_index) DO UPDATE SET
                    content = EXCLUDED.content,
                    start_line = EXCLUDED.start_line,
                    end_line = EXCLUDED.end_line,
                    embedding_v2 = EXCLUDED.embedding_v2,
                    embedding_signature = EXCLUDED.embedding_signature",
            )
            .bind(file_id)
            .bind(chunk.chunk_index)
            .bind(chunk.content)
            .bind(chunk.start_line)
            .bind(chunk.end_line)
            .bind(embedding_vec)
            .execute(&mut *tx)
            .await?;
        }

        sqlx::query("UPDATE indexed_files SET content_hash = $1 WHERE id = $2")
            .bind(replacement.content_hash)
            .bind(file_id)
            .execute(&mut *tx)
            .await?;

        Ok::<i64, sqlx::Error>(file_id)
    }
    .await;

    match res {
        Ok(file_id) => {
            tx.commit().await?;
            Ok(file_id)
        }
        Err(e) => {
            let _ = tx.rollback().await;
            Err(e)
        }
    }
}

/// Outcome of a batched chunk insert.
///
/// The batch runs inside a single transaction so the embed pool holds
/// one pooled connection across all N inserts instead of N separate
/// acquisitions. Semantics:
///
/// - `Ok` with `fk_violation == false` and empty `error`: every chunk
///   committed; the caller proceeds to `finalize_file_hash`.
/// - `Ok` with `fk_violation == true`: the parent row was deleted
///   mid-batch (PG SQLSTATE 23503); the transaction is rolled back so
///   no orphan rows land. The caller logs once and increments
///   `files_aborted_fk` (this matches the prior per-chunk
///   `AbortedFk` outcome).
/// - `Ok` with `error == Some`: a non-FK error fired; the transaction
///   is rolled back to keep the file all-or-nothing rather than leaving
///   a partial chunk set under a NULL content_hash. The caller logs
///   the error and counts the file as failed; the next rescan will
///   re-attempt.
pub struct ChunkBatchOutcome {
    pub fk_violation: bool,
    pub error: Option<sqlx::Error>,
}

/// Insert N chunks for a file inside a single transaction.
///
/// All chunks land or none do — this trades the prior loop's
/// "commit successful chunks even if one fails" behavior for cleaner
/// all-or-nothing semantics. An incomplete file is detectable from
/// outside as a row with `content_hash = NULL` and zero `file_chunks`
/// rows, the same shape the integrity-check cron already handles, but
/// without partial chunks wasting storage in the meantime.
pub async fn insert_chunks_batch(
    pool: &PgPool,
    file_id: i64,
    chunks: &[ChunkInsert<'_>],
) -> Result<ChunkBatchOutcome, sqlx::Error> {
    if chunks.is_empty() {
        return Ok(ChunkBatchOutcome {
            fk_violation: false,
            error: None,
        });
    }
    let mut tx = pool.begin().await?;
    for chunk in chunks {
        // BGE-M3-only: chunks are written to the 1024-d `embedding_v2`
        // column. Same shape as insert_chunk; a non-1024 dim is a
        // configuration error reported back through the batch outcome.
        let embedding_vec = pgvector::Vector::from(chunk.embedding.to_vec());
        if chunk.embedding.len() != 1024 {
            drop(tx);
            return Ok(ChunkBatchOutcome {
                fk_violation: false,
                error: Some(sqlx::Error::Protocol(format!(
                    "insert_chunks_batch: expected a 1024-dimension BGE-M3 \
                     embedding, got {}",
                    chunk.embedding.len()
                ))),
            });
        }
        let sql = "INSERT INTO file_chunks
                    (file_id, chunk_index, content, start_line, end_line,
                     embedding_v2, embedding_signature)
                 VALUES ($1, $2, $3, $4, $5, $6, 'bge-m3-v1')
                 ON CONFLICT (file_id, chunk_index) DO UPDATE SET
                    content = EXCLUDED.content,
                    start_line = EXCLUDED.start_line,
                    end_line = EXCLUDED.end_line,
                    embedding_v2 = EXCLUDED.embedding_v2,
                    embedding_signature = EXCLUDED.embedding_signature";
        match sqlx::query(sql)
            .bind(file_id)
            .bind(chunk.chunk_index)
            .bind(chunk.content)
            .bind(chunk.start_line)
            .bind(chunk.end_line)
            .bind(embedding_vec)
            .execute(&mut *tx)
            .await
        {
            Ok(_) => {}
            Err(e) => {
                let fk = matches!(
                    &e,
                    sqlx::Error::Database(db_err) if db_err.code().as_deref() == Some("23503")
                );
                // Dropping the transaction without `commit()` rolls back.
                drop(tx);
                return Ok(ChunkBatchOutcome {
                    fk_violation: fk,
                    error: if fk { None } else { Some(e) },
                });
            }
        }
    }
    tx.commit().await?;
    Ok(ChunkBatchOutcome {
        fk_violation: false,
        error: None,
    })
}

/// One chunk plus its deterministic context inputs, for the contextual
/// re-embed cron (graph-roadmap Phase 2.4). The enclosing symbol is the
/// innermost `file_symbols` row whose line span contains the chunk.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ChunkContextRow {
    pub id: i64,
    pub content: String,
    pub relative_path: String,
    pub language: String,
    pub symbol_kind: Option<String>,
    pub symbol_name: Option<String>,
    pub symbol_signature: Option<String>,
    pub importer_count: i64,
}

/// Drain up to `batch_size` `file_chunks` that have a dense `embedding_v2` but
/// no `contextual_text` yet, with their context inputs (enclosing symbol via a
/// line-span LATERAL, importer count from import edges). `FOR UPDATE OF c SKIP
/// LOCKED` makes concurrent passes safe.
pub async fn get_chunks_needing_context(
    pool: &PgPool,
    batch_size: i32,
) -> Result<Vec<ChunkContextRow>, sqlx::Error> {
    sqlx::query_as::<_, ChunkContextRow>(
        "SELECT c.id, c.content, f.relative_path, f.language,
                sym.kind AS symbol_kind, sym.name AS symbol_name,
                sym.signature AS symbol_signature,
                COALESCE(imp.cnt, 0) AS importer_count
         FROM file_chunks c
         JOIN indexed_files f ON f.id = c.file_id
         LEFT JOIN LATERAL (
             SELECT fs.kind, fs.name, fs.signature
             FROM file_symbols fs
             WHERE fs.file_id = c.file_id
               AND fs.start_line <= c.start_line
               AND fs.end_line >= c.end_line
               AND fs.kind IN ('function','method','class','struct','impl','trait','interface','enum','module')
             ORDER BY fs.start_line DESC
             LIMIT 1
         ) sym ON true
         LEFT JOIN LATERAL (
             SELECT COUNT(*) AS cnt
             FROM code_graph_edges e
             WHERE e.target_file_id = c.file_id AND e.edge_type = 'import'
         ) imp ON true
         WHERE c.contextual_text IS NULL AND c.embedding_v2 IS NOT NULL
         ORDER BY c.id
         LIMIT $1
         FOR UPDATE OF c SKIP LOCKED",
    )
    .bind(batch_size as i64)
    .fetch_all(pool)
    .await
}

/// One chunk's content + line range, used for centroid selection and
/// snippet display in chunk_clusters / boilerplate_clusters.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ChunkContentRow {
    pub chunk_id: i64,
    pub file_id: i64,
    pub start_line: i32,
    pub end_line: i32,
    pub content: String,
}

/// Fetch chunk content + line ranges for a set of chunk_ids. Used by
/// chunk-cluster tools after clustering, to emit per-member snippets and
/// estimate `loc_per_chunk_avg`.
///
/// Tolerates FK drift (chunk deleted mid-run): rows that no longer exist
/// are simply absent from the result. The caller skips clusters with
/// fewer-than-expected returned rows.
pub async fn get_chunk_content_rows(
    pool: &PgPool,
    chunk_ids: &[i64],
) -> Result<Vec<ChunkContentRow>, sqlx::Error> {
    if chunk_ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as::<_, ChunkContentRow>(
        "SELECT id AS chunk_id, file_id, start_line, end_line, content
         FROM file_chunks
         WHERE id = ANY($1)",
    )
    .bind(chunk_ids)
    .fetch_all(pool)
    .await
}

/// One chunk's content + line + chunk-index info. Returned by the new
/// region-read helpers used by `read_file`'s `start_line`/`end_line` and
/// `chunk_index_start`/`chunk_index_end` parameters.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct FileChunkRow {
    pub chunk_index: i32,
    pub start_line: i32,
    pub end_line: i32,
    pub content: String,
}

/// Fetch all chunks of a file (by path) whose `(start_line, end_line)`
/// range overlaps the requested `[start, end]` line window. Returns
/// chunks ordered by `chunk_index`. Used by `read_file` when the caller
/// supplies a line range — avoids loading the entire file content for a
/// targeted slice and works even when `indexed_files.content` is NULL
/// (Level-1 oversized files).
///
/// Follows `duplicate_of_file_id` so duplicate-pointer rows return their
/// canonical's chunks transparently.
pub async fn get_file_region_by_lines(
    pool: &PgPool,
    path: &str,
    start_line: i32,
    end_line: i32,
) -> Result<Vec<FileChunkRow>, sqlx::Error> {
    let rows = sqlx::query_as::<_, FileChunkRow>(
        "SELECT c.chunk_index, c.start_line, c.end_line, c.content
         FROM file_chunks c
         JOIN indexed_files f ON c.file_id = COALESCE(f.duplicate_of_file_id, f.id)
         WHERE f.path = $1
           AND c.start_line <= $3
           AND c.end_line   >= $2
         ORDER BY c.chunk_index",
    )
    .bind(path)
    .bind(start_line)
    .bind(end_line)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Fetch chunks of a file by `chunk_index` range, inclusive. Returns
/// chunks ordered by `chunk_index`. Follows `duplicate_of_file_id`.
pub async fn get_chunks_in_index_range(
    pool: &PgPool,
    path: &str,
    idx_start: i32,
    idx_end: i32,
) -> Result<Vec<FileChunkRow>, sqlx::Error> {
    let rows = sqlx::query_as::<_, FileChunkRow>(
        "SELECT c.chunk_index, c.start_line, c.end_line, c.content
         FROM file_chunks c
         JOIN indexed_files f ON c.file_id = COALESCE(f.duplicate_of_file_id, f.id)
         WHERE f.path = $1
           AND c.chunk_index >= $2
           AND c.chunk_index <= $3
         ORDER BY c.chunk_index",
    )
    .bind(path)
    .bind(idx_start)
    .bind(idx_end)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Aggregate metadata about a file's chunks. Used by `file_info` to
/// report `chunk_count`, `first_chunk_line`, and `last_chunk_line` so
/// clients can size region reads without an extra round-trip.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct FileChunkSummary {
    pub chunk_count: i32,
    pub first_chunk_line: Option<i32>,
    pub last_chunk_line: Option<i32>,
}

/// Fetch chunk count + line span for a file. Returns zeros for files
/// without any chunks (oversized Level-1 placeholders, empty files).
/// Follows `duplicate_of_file_id` so a duplicate reports its canonical's
/// chunk metadata.
pub async fn file_chunk_summary(
    pool: &PgPool,
    path: &str,
) -> Result<FileChunkSummary, sqlx::Error> {
    let row = sqlx::query_as::<_, FileChunkSummary>(
        "SELECT
            COUNT(*)::int AS chunk_count,
            MIN(c.start_line) AS first_chunk_line,
            MAX(c.end_line) AS last_chunk_line
         FROM file_chunks c
         JOIN indexed_files f ON c.file_id = COALESCE(f.duplicate_of_file_id, f.id)
         WHERE f.path = $1",
    )
    .bind(path)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// Per-chunk topic summary used to derive cluster keywords.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ChunkTopicSummaryRow {
    pub chunk_id: i64,
    pub topic_id: i64,
    pub label: String,
    pub keywords: Option<Vec<String>>,
    pub membership_score: f64,
}

/// Fetch topic summaries (label + keyword list) for the chunks in a cluster,
/// joining `chunk_topic_assignments` to `code_topics`. Returns one row per
/// (chunk, topic) pairing — chunks may be members of multiple topics under FCM.
///
/// If topics haven't been computed (`code_topics` empty), returns empty;
/// callers fall back to identifier-token heuristics.
pub async fn get_chunk_topic_summaries(
    pool: &PgPool,
    chunk_ids: &[i64],
) -> Result<Vec<ChunkTopicSummaryRow>, sqlx::Error> {
    if chunk_ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as::<_, ChunkTopicSummaryRow>(
        // `code_topics.id` is `integer` (INT4); cast to BIGINT so it decodes into
        // the `topic_id: i64` field (sqlx rejects INT4->i64).
        "SELECT cta.chunk_id, ct.id::bigint AS topic_id, ct.label, ct.keywords, cta.membership_score
         FROM chunk_topic_assignments cta
         JOIN code_topics ct ON ct.id = cta.topic_id
         WHERE cta.chunk_id = ANY($1)
           AND cta.membership_score >= 0.05
         ORDER BY cta.chunk_id, cta.membership_score DESC",
    )
    .bind(chunk_ids)
    .fetch_all(pool)
    .await
}

// ============================================================================
// Topic clustering queries
// ============================================================================

/// A chunk row with its embedding for bulk extraction.
#[derive(Debug, Clone)]
pub struct ChunkEmbeddingRow {
    pub chunk_id: i64,
    pub file_id: i64,
    pub project_id: i32,
    pub project_name: String,
    pub path: String,
    pub language: String,
    pub content: String,
    pub embedding: Vec<f32>,
}

/// Extract all chunk embeddings, optionally filtered by language.
///
/// Worktree-aware: when multiple projects share a `git_common_dir` (or
/// `git_root_commits`), only the canonical project (smallest
/// `projects.id`) contributes chunks. Otherwise the global topic scan
/// would double- or triple-count the same code per worktree, inflating
/// `code_topics.project_count` / `project_names` / chunk counts. The
/// per-project topic scan (`bulk_extract_project_embeddings`) doesn't
/// need this filter — already scoped to one project by name.
pub async fn bulk_extract_embeddings(
    pool: &PgPool,
    language: Option<&str>,
) -> Result<Vec<ChunkEmbeddingRow>, sqlx::Error> {
    // Phase 5 C8: dispatch on active embedding signature. Topic
    // clustering's centroids are dim-agnostic (stored as untyped
    // REAL[]) so the only thing this affects is which column we
    // SELECT from.
    let active = crate::embed::signature::read_active_signature(pool).await?;
    let col = active.read_column();
    let canonical_filter = "NOT EXISTS (
        SELECT 1 FROM projects p_dup
        WHERE p_dup.id < p.id
          AND (
              (p_dup.git_common_dir IS NOT NULL
               AND p.git_common_dir IS NOT NULL
               AND p_dup.git_common_dir = p.git_common_dir)
              OR
              (p_dup.git_root_commits IS NOT NULL
               AND p.git_root_commits IS NOT NULL
               AND p_dup.git_root_commits = p.git_root_commits)
          )
    )";
    if let Some(lang) = language {
        let rows = sqlx::query_as::<_, BulkChunkRow>(sqlx::AssertSqlSafe(format!(
            "SELECT c.id as chunk_id, c.file_id, f.project_id, p.name as project_name,
                    f.path, f.language, c.content, c.{col}::real[] as embedding
             FROM file_chunks c
             JOIN indexed_files f ON f.id = c.file_id
             JOIN projects p ON p.id = f.project_id
             WHERE f.language = $1
               AND c.{col} IS NOT NULL
               AND {canonical_filter}
             ORDER BY c.id",
        )))
        .bind(lang)
        .fetch_all(pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    } else {
        let rows = sqlx::query_as::<_, BulkChunkRow>(sqlx::AssertSqlSafe(format!(
            "SELECT c.id as chunk_id, c.file_id, f.project_id, p.name as project_name,
                    f.path, f.language, c.content, c.{col}::real[] as embedding
             FROM file_chunks c
             JOIN indexed_files f ON f.id = c.file_id
             JOIN projects p ON p.id = f.project_id
             WHERE c.{col} IS NOT NULL
               AND {canonical_filter}
             ORDER BY c.id",
        )))
        .fetch_all(pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }
}

/// Extract chunk embeddings for a specific project, optionally filtered by language.
pub async fn bulk_extract_project_embeddings(
    pool: &PgPool,
    project_name: &str,
    language: Option<&str>,
) -> Result<Vec<ChunkEmbeddingRow>, sqlx::Error> {
    // Phase 5 C8: signature-aware column.
    let active = crate::embed::signature::read_active_signature(pool).await?;
    let col = active.read_column();
    if let Some(lang) = language {
        let rows = sqlx::query_as::<_, BulkChunkRow>(sqlx::AssertSqlSafe(format!(
            "SELECT c.id as chunk_id, c.file_id, f.project_id, p.name as project_name,
                    f.path, f.language, c.content, c.{col}::real[] as embedding
             FROM file_chunks c
             JOIN indexed_files f ON f.id = c.file_id
             JOIN projects p ON p.id = f.project_id
             WHERE p.name = $1 AND f.language = $2
               AND c.{col} IS NOT NULL
             ORDER BY c.id"
        )))
        .bind(project_name)
        .bind(lang)
        .fetch_all(pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    } else {
        let rows = sqlx::query_as::<_, BulkChunkRow>(sqlx::AssertSqlSafe(format!(
            "SELECT c.id as chunk_id, c.file_id, f.project_id, p.name as project_name,
                    f.path, f.language, c.content, c.{col}::real[] as embedding
             FROM file_chunks c
             JOIN indexed_files f ON f.id = c.file_id
             JOIN projects p ON p.id = f.project_id
             WHERE p.name = $1
               AND c.{col} IS NOT NULL
             ORDER BY c.id"
        )))
        .bind(project_name)
        .fetch_all(pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }
}

/// Internal sqlx-compatible row for bulk chunk extraction.
/// The `embedding` column uses `::real[]` cast so sqlx maps it to `Vec<f32>`.
#[derive(Debug, sqlx::FromRow)]
struct BulkChunkRow {
    chunk_id: i64,
    file_id: i64,
    project_id: i32,
    project_name: String,
    path: String,
    language: String,
    content: String,
    embedding: Vec<f32>,
}

impl From<BulkChunkRow> for ChunkEmbeddingRow {
    fn from(row: BulkChunkRow) -> Self {
        Self {
            chunk_id: row.chunk_id,
            file_id: row.file_id,
            project_id: row.project_id,
            project_name: row.project_name,
            path: row.path,
            language: row.language,
            content: row.content,
            embedding: row.embedding,
        }
    }
}

/// The single best-matching chunk per file (by cosine to `embedding`) for the
/// given `file_ids`. Used to materialize code-PPR results after the graph walk
/// re-ranks files. (Phase 3.3)
pub async fn best_chunk_per_file(
    pool: &PgPool,
    embedding: &[f32],
    file_ids: &[i64],
) -> Result<Vec<PprFileChunk>, sqlx::Error> {
    if file_ids.is_empty() {
        return Ok(Vec::new());
    }
    let col = embedding_column_for_dim(embedding.len())?;
    let embedding_vec = pgvector::Vector::from(embedding.to_vec());
    sqlx::query_as::<_, PprFileChunk>(sqlx::AssertSqlSafe(format!(
        "SELECT DISTINCT ON (c.file_id)
                c.file_id, f.relative_path, f.language, c.content,
                c.start_line, c.end_line, (1.0 - (c.{col} <=> $1))::float8 AS similarity
         FROM file_chunks c
         JOIN indexed_files f ON f.id = c.file_id
         WHERE c.file_id = ANY($2) AND c.{col} IS NOT NULL
         ORDER BY c.file_id, c.{col} <=> $1"
    )))
    .bind(embedding_vec)
    .bind(file_ids)
    .fetch_all(pool)
    .await
}

/// Per-candidate re-ranking features (graph-roadmap Phase 4.2): the embedding
/// (for MMR diversity) and last-change date (for the recency prior), keyed by
/// chunk id. `embedding`/`blame_date` may be NULL for un-backfilled chunks.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ChunkRerankFeature {
    pub chunk_id: i64,
    pub embedding: Option<pgvector::Vector>,
    pub blame_date: Option<chrono::DateTime<chrono::Utc>>,
}

/// Fetch MMR/recency features for a set of chunk ids. `query_dim` selects the
/// active embedding column (1024 = BGE-M3 `embedding_v2`) so the returned
/// vectors share the query's space.
pub async fn chunk_rerank_features(
    pool: &PgPool,
    chunk_ids: &[i64],
    query_dim: usize,
) -> Result<Vec<ChunkRerankFeature>, sqlx::Error> {
    if chunk_ids.is_empty() {
        return Ok(Vec::new());
    }
    let col = embedding_column_for_dim(query_dim)?;
    sqlx::query_as::<_, ChunkRerankFeature>(sqlx::AssertSqlSafe(format!(
        "SELECT id AS chunk_id, {col} AS embedding, blame_date
         FROM file_chunks WHERE id = ANY($1)"
    )))
    .bind(chunk_ids)
    .fetch_all(pool)
    .await
}
