//! Cross-project similarity queries (real-time file/chunk comparison,
//! batch neighbor scan + pair persistence, duplicate/abstraction discovery).
//! Extracted from `queries.rs` (god-file split).
#![allow(unused_imports)]

use crate::db::queries::*;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

/// A pair of chunks with their similarity score (for file comparison).
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct ChunkPairSimilarity {
    pub chunk_id_a: i64,
    pub content_a: String,
    pub start_line_a: i32,
    pub end_line_a: i32,
    pub chunk_id_b: i64,
    pub content_b: String,
    pub start_line_b: i32,
    pub end_line_b: i32,
    pub similarity: f64,
}

/// Compare two files by cross-joining their chunks and computing pairwise similarity.
/// Returns all chunk pairs sorted by similarity descending.
pub async fn compare_two_files(
    pool: &PgPool,
    file_id_a: i64,
    file_id_b: i64,
    ef_search: i32,
) -> Result<Vec<ChunkPairSimilarity>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(&format!("SET LOCAL hnsw.ef_search = {}", ef_search))
        .execute(&mut *tx)
        .await?;

    // Post-cutover the legacy `embedding` column is gone; read the active
    // signature's column (embedding_v2 under BGE-M3).
    let col = crate::embed::signature::read_active_signature(pool)
        .await?
        .read_column();
    let results = sqlx::query_as::<_, ChunkPairSimilarity>(&format!(
        "SELECT ca.id as chunk_id_a, ca.content as content_a,
                ca.start_line as start_line_a, ca.end_line as end_line_a,
                cb.id as chunk_id_b, cb.content as content_b,
                cb.start_line as start_line_b, cb.end_line as end_line_b,
                1 - (ca.{col} <=> cb.{col}) as similarity
         FROM file_chunks ca
         CROSS JOIN file_chunks cb
         WHERE ca.file_id = $1 AND cb.file_id = $2
         ORDER BY similarity DESC",
    ))
    .bind(file_id_a)
    .bind(file_id_b)
    .fetch_all(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(results)
}

/// Real-time intra-file chunk-pair similarity. Used by `internal_dry`.
///
/// Cross-joins `file_chunks` against itself with `c.id < c'.id` to avoid
/// the symmetric duplicate, returning rows where the cosine similarity
/// is at least `min_similarity`. Single-file variant of `compare_two_files`.
pub async fn compare_chunks_within_file(
    pool: &PgPool,
    file_id: i64,
    min_similarity: f64,
    ef_search: i32,
) -> Result<Vec<ChunkPairSimilarity>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(&format!("SET LOCAL hnsw.ef_search = {}", ef_search))
        .execute(&mut *tx)
        .await?;

    let col = crate::embed::signature::read_active_signature(pool)
        .await?
        .read_column();
    let results = sqlx::query_as::<_, ChunkPairSimilarity>(&format!(
        "SELECT ca.id AS chunk_id_a, ca.content AS content_a,
                ca.start_line AS start_line_a, ca.end_line AS end_line_a,
                cb.id AS chunk_id_b, cb.content AS content_b,
                cb.start_line AS start_line_b, cb.end_line AS end_line_b,
                1 - (ca.{col} <=> cb.{col}) AS similarity
         FROM file_chunks ca
         JOIN file_chunks cb
              ON ca.file_id = cb.file_id AND ca.id < cb.id
         WHERE ca.file_id = $1
           AND 1 - (ca.{col} <=> cb.{col}) >= $2
         ORDER BY similarity DESC",
    ))
    .bind(file_id)
    .bind(min_similarity)
    .fetch_all(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(results)
}

/// Row returned by the batch similarity scanner.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SimilarityNeighborRow {
    pub chunk_id_a: i64,
    pub file_id_a: i64,
    pub project_id_a: i32,
    pub path_a: String,
    pub project_name_a: String,
    pub language: String,
    pub chunk_id_b: i64,
    pub file_id_b: i64,
    pub project_id_b: i32,
    pub path_b: String,
    pub project_name_b: String,
    pub similarity: f64,
}

#[derive(Debug)]
struct SimilarityPairInsert<'a> {
    chunk_id_a: i64,
    file_id_a: i64,
    project_id_a: i32,
    path_a: &'a str,
    project_name_a: &'a str,
    chunk_id_b: i64,
    file_id_b: i64,
    project_id_b: i32,
    path_b: &'a str,
    project_name_b: &'a str,
    similarity: f64,
    language: &'a str,
}

fn normalize_similarity_pair(row: &SimilarityNeighborRow) -> SimilarityPairInsert<'_> {
    if row.chunk_id_a <= row.chunk_id_b {
        SimilarityPairInsert {
            chunk_id_a: row.chunk_id_a,
            file_id_a: row.file_id_a,
            project_id_a: row.project_id_a,
            path_a: &row.path_a,
            project_name_a: &row.project_name_a,
            chunk_id_b: row.chunk_id_b,
            file_id_b: row.file_id_b,
            project_id_b: row.project_id_b,
            path_b: &row.path_b,
            project_name_b: &row.project_name_b,
            similarity: row.similarity,
            language: &row.language,
        }
    } else {
        SimilarityPairInsert {
            chunk_id_a: row.chunk_id_b,
            file_id_a: row.file_id_b,
            project_id_a: row.project_id_b,
            path_a: &row.path_b,
            project_name_a: &row.project_name_b,
            chunk_id_b: row.chunk_id_a,
            file_id_b: row.file_id_a,
            project_id_b: row.project_id_a,
            path_b: &row.path_a,
            project_name_b: &row.project_name_a,
            similarity: row.similarity,
            language: &row.language,
        }
    }
}

/// Find cross-project nearest neighbors for a batch of chunks.
/// Uses CROSS JOIN LATERAL with the HNSW index for efficient ANN lookups.
/// Returns rows with chunk_id_a from the batch and their nearest cross-project neighbors.
pub async fn batch_find_cross_project_neighbors(
    pool: &PgPool,
    last_chunk_id: i64,
    batch_size: i32,
    top_k: i32,
    threshold: f64,
    ef_search: i32,
) -> Result<Vec<SimilarityNeighborRow>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(&format!("SET LOCAL hnsw.ef_search = {}", ef_search))
        .execute(&mut *tx)
        .await?;
    // The cross-project nearest-neighbor batch can legitimately run
    // longer than the daemon-wide statement_timeout on large indexes
    // (HNSW scan × 500-row batches). Raise the ceiling for this
    // transaction only.
    sqlx::query("SET LOCAL statement_timeout = '5min'")
        .execute(&mut *tx)
        .await?;
    // Label this heavy transaction so the graceful-shutdown sweep
    // (db::admin::terminate_heavy_backends) can terminate it and free its locks.
    sqlx::query("SET LOCAL application_name = 'pgmcp:heavy:similarity-scan'")
        .execute(&mut *tx)
        .await?;

    // Worktree-awareness: skip pairs whose two projects are different
    // worktrees / sibling clones of the same upstream repo (same
    // git_common_dir OR same git_root_commits). Otherwise the
    // materialized similarity table fills with same-code-different-branch
    // false positives that drown out genuine cross-repo refactor candidates.
    // See plan: ~/.claude/plans/thoroughly-examine-home-dylon-workspace-melodic-cake.md
    let col = crate::embed::signature::read_active_signature(pool)
        .await?
        .read_column();
    let results = sqlx::query_as::<_, SimilarityNeighborRow>(&format!(
        "WITH batch AS (
            SELECT c.id, c.file_id, c.{col} AS embedding, f.project_id, f.path, f.language,
                   p.name as project_name,
                   p.git_common_dir as git_common_dir_b,
                   p.git_root_commits as git_root_commits_b
            FROM file_chunks c
            JOIN indexed_files f ON f.id = c.file_id
            JOIN projects p ON p.id = f.project_id
            WHERE c.id > $1
            ORDER BY c.id
            LIMIT $2
        )
        SELECT b.id as chunk_id_a, b.file_id as file_id_a, b.project_id as project_id_a,
               b.path as path_a, b.project_name as project_name_a, b.language,
               nn.chunk_id_b, nn.file_id_b, nn.project_id_b, nn.path_b, nn.project_name_b,
               nn.similarity
        FROM batch b
        CROSS JOIN LATERAL (
            SELECT c2.id as chunk_id_b, c2.file_id as file_id_b, f2.project_id as project_id_b,
                   f2.path as path_b, p2.name as project_name_b,
                   1 - (c2.{col} <=> b.embedding) as similarity
            FROM file_chunks c2
            JOIN indexed_files f2 ON f2.id = c2.file_id
            JOIN projects p2 ON p2.id = f2.project_id
            WHERE f2.project_id != b.project_id
              AND NOT (
                  p2.git_common_dir IS NOT NULL
                  AND p2.git_common_dir = b.git_common_dir_b
              )
              AND NOT (
                  p2.git_root_commits IS NOT NULL
                  AND p2.git_root_commits = b.git_root_commits_b
              )
            ORDER BY c2.{col} <=> b.embedding
            LIMIT $3
        ) nn
        WHERE nn.similarity >= $4",
    ))
    .bind(last_chunk_id)
    .bind(batch_size)
    .bind(top_k)
    .bind(threshold)
    .fetch_all(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(results)
}

/// Insert a batch of similarity pairs into the cross_project_similarities table.
/// Normalizes pair ordering so chunk_id_a < chunk_id_b.
/// Uses ON CONFLICT DO UPDATE to upsert when we find a better similarity score.
pub async fn insert_similarity_pairs(
    pool: &PgPool,
    rows: &[SimilarityNeighborRow],
) -> Result<u64, sqlx::Error> {
    if rows.is_empty() {
        return Ok(0);
    }

    let mut inserted = 0u64;
    for row in rows {
        let pair = normalize_similarity_pair(row);

        let result = sqlx::query(
            "INSERT INTO cross_project_similarities
                (chunk_id_a, file_id_a, project_id_a, chunk_id_b, file_id_b, project_id_b,
                 chunk_similarity, path_a, path_b, project_name_a, project_name_b, language)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
             ON CONFLICT (chunk_id_a, chunk_id_b) DO UPDATE SET
                chunk_similarity = GREATEST(cross_project_similarities.chunk_similarity, EXCLUDED.chunk_similarity)"
        )
        .bind(pair.chunk_id_a)
        .bind(pair.file_id_a)
        .bind(pair.project_id_a)
        .bind(pair.chunk_id_b)
        .bind(pair.file_id_b)
        .bind(pair.project_id_b)
        .bind(pair.similarity)
        .bind(pair.path_a)
        .bind(pair.path_b)
        .bind(pair.project_name_a)
        .bind(pair.project_name_b)
        .bind(pair.language)
        .execute(pool)
        .await?;

        inserted += result.rows_affected();
    }

    Ok(inserted)
}

#[cfg(test)]
mod similarity_pair_tests {
    use super::{SimilarityNeighborRow, normalize_similarity_pair};

    fn row(chunk_id_a: i64, chunk_id_b: i64) -> SimilarityNeighborRow {
        SimilarityNeighborRow {
            chunk_id_a,
            file_id_a: 10,
            project_id_a: 100,
            path_a: "src/a.rs".to_string(),
            project_name_a: "project-a".to_string(),
            language: "rust".to_string(),
            chunk_id_b,
            file_id_b: 20,
            project_id_b: 200,
            path_b: "src/b.rs".to_string(),
            project_name_b: "project-b".to_string(),
            similarity: 0.91,
        }
    }

    #[test]
    fn similarity_pair_bind_order_keeps_paths_and_projects_distinct() {
        let input = row(1, 2);
        let pair = normalize_similarity_pair(&input);

        assert_eq!(pair.chunk_id_a, 1);
        assert_eq!(pair.path_a, "src/a.rs");
        assert_eq!(pair.path_b, "src/b.rs");
        assert_eq!(pair.project_name_a, "project-a");
        assert_eq!(pair.project_name_b, "project-b");
    }

    #[test]
    fn similarity_pair_normalization_swaps_all_metadata_together() {
        let input = row(2, 1);
        let pair = normalize_similarity_pair(&input);

        assert_eq!(pair.chunk_id_a, 1);
        assert_eq!(pair.file_id_a, 20);
        assert_eq!(pair.project_id_a, 200);
        assert_eq!(pair.path_a, "src/b.rs");
        assert_eq!(pair.project_name_a, "project-b");
        assert_eq!(pair.chunk_id_b, 2);
        assert_eq!(pair.file_id_b, 10);
        assert_eq!(pair.project_id_b, 100);
        assert_eq!(pair.path_b, "src/a.rs");
        assert_eq!(pair.project_name_b, "project-a");
    }
}

/// Clear the cross_project_similarities table (before a fresh scan).
pub async fn clear_similarity_table(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query("TRUNCATE cross_project_similarities")
        .execute(pool)
        .await?;
    Ok(())
}

/// Count total similarity pairs in the materialized table.
pub async fn count_similarity_pairs(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM cross_project_similarities")
        .fetch_one(pool)
        .await?;
    Ok(count as u64)
}

/// Get the top N file pairs by average similarity from the materialized table.
pub async fn top_similar_file_pairs(
    pool: &PgPool,
    limit: i32,
) -> Result<Vec<FileSimilarityPair>, sqlx::Error> {
    sqlx::query_as::<_, FileSimilarityPair>(
        "SELECT s.file_id_a, s.path_a, s.project_name_a,
                s.file_id_b, s.path_b, s.project_name_b,
                s.language,
                AVG(s.chunk_similarity) as avg_similarity,
                MAX(s.chunk_similarity) as max_similarity,
                COUNT(*) as matching_chunks
         FROM cross_project_similarities s
         GROUP BY s.file_id_a, s.path_a, s.project_name_a,
                  s.file_id_b, s.path_b, s.project_name_b, s.language
         ORDER BY avg_similarity DESC
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Get the max chunk_id in the file_chunks table (for batch iteration).
pub async fn max_chunk_id(pool: &PgPool) -> Result<i64, sqlx::Error> {
    let id = sqlx::query_scalar::<_, Option<i64>>("SELECT MAX(id) FROM file_chunks")
        .fetch_one(pool)
        .await?;
    Ok(id.unwrap_or(0))
}

/// File-level similarity pair from the materialized table.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct FileSimilarityPair {
    pub file_id_a: i64,
    pub path_a: String,
    pub project_name_a: String,
    pub file_id_b: i64,
    pub path_b: String,
    pub project_name_b: String,
    pub language: String,
    pub avg_similarity: f64,
    pub max_similarity: f64,
    pub matching_chunks: i64,
}

/// Find files similar to a given file from the materialized similarity table.
/// Aggregates chunk-level similarity to file-level.
pub async fn find_similar_files(
    pool: &PgPool,
    file_id: i64,
    min_similarity: f64,
    limit: i32,
    target_project: Option<&str>,
    include_same_repo: bool,
) -> Result<Vec<FileSimilarityPair>, sqlx::Error> {
    // Same-repo filter: exclude rows where the *other* side's project is
    // a worktree / sibling clone of the seed file's project. The seed's
    // project is `seed.project_id` (looked up via indexed_files); the
    // other side is whichever of project_id_a/project_id_b isn't seed's.
    // The leading `$N OR …` short-circuits the filter when the operator
    // explicitly opts in via `include_same_repo=true`.
    let same_repo_filter_idx = if target_project.is_some() { "$5" } else { "$4" };
    let same_repo_filter = format!(
        "({} OR NOT EXISTS (
            SELECT 1 FROM projects pa, projects pb, indexed_files seed
            WHERE seed.id = $1
              AND pa.id = seed.project_id
              AND pb.id = (CASE WHEN s.file_id_a = $1 THEN s.project_id_b ELSE s.project_id_a END)
              AND (
                  (pa.git_common_dir IS NOT NULL AND pa.git_common_dir = pb.git_common_dir)
                  OR
                  (pa.git_root_commits IS NOT NULL AND pa.git_root_commits = pb.git_root_commits)
              )
         ))",
        same_repo_filter_idx
    );
    if let Some(proj) = target_project {
        sqlx::query_as::<_, FileSimilarityPair>(&format!(
            "SELECT
                CASE WHEN s.file_id_a = $1 THEN s.file_id_a ELSE s.file_id_b END as file_id_a,
                CASE WHEN s.file_id_a = $1 THEN s.path_a ELSE s.path_b END as path_a,
                CASE WHEN s.file_id_a = $1 THEN s.project_name_a ELSE s.project_name_b END as project_name_a,
                CASE WHEN s.file_id_a = $1 THEN s.file_id_b ELSE s.file_id_a END as file_id_b,
                CASE WHEN s.file_id_a = $1 THEN s.path_b ELSE s.path_a END as path_b,
                CASE WHEN s.file_id_a = $1 THEN s.project_name_b ELSE s.project_name_a END as project_name_b,
                s.language,
                AVG(s.chunk_similarity) as avg_similarity,
                MAX(s.chunk_similarity) as max_similarity,
                COUNT(*) as matching_chunks
             FROM cross_project_similarities s
             WHERE (s.file_id_a = $1 OR s.file_id_b = $1)
               AND s.chunk_similarity >= $2
               AND (CASE WHEN s.file_id_a = $1 THEN s.project_name_b ELSE s.project_name_a END) = $4
               AND {same_repo_filter}
             GROUP BY file_id_a, path_a, project_name_a, file_id_b, path_b, project_name_b, s.language
             ORDER BY avg_similarity DESC
             LIMIT $3"
        ))
        .bind(file_id)
        .bind(min_similarity)
        .bind(limit)
        .bind(proj)
        .bind(include_same_repo)
        .fetch_all(pool)
        .await
    } else {
        sqlx::query_as::<_, FileSimilarityPair>(&format!(
            "SELECT
                CASE WHEN s.file_id_a = $1 THEN s.file_id_a ELSE s.file_id_b END as file_id_a,
                CASE WHEN s.file_id_a = $1 THEN s.path_a ELSE s.path_b END as path_a,
                CASE WHEN s.file_id_a = $1 THEN s.project_name_a ELSE s.project_name_b END as project_name_a,
                CASE WHEN s.file_id_a = $1 THEN s.file_id_b ELSE s.file_id_a END as file_id_b,
                CASE WHEN s.file_id_a = $1 THEN s.path_b ELSE s.path_a END as path_b,
                CASE WHEN s.file_id_a = $1 THEN s.project_name_b ELSE s.project_name_a END as project_name_b,
                s.language,
                AVG(s.chunk_similarity) as avg_similarity,
                MAX(s.chunk_similarity) as max_similarity,
                COUNT(*) as matching_chunks
             FROM cross_project_similarities s
             WHERE (s.file_id_a = $1 OR s.file_id_b = $1)
               AND s.chunk_similarity >= $2
               AND {same_repo_filter}
             GROUP BY file_id_a, path_a, project_name_a, file_id_b, path_b, project_name_b, s.language
             ORDER BY avg_similarity DESC
             LIMIT $3"
        ))
        .bind(file_id)
        .bind(min_similarity)
        .bind(limit)
        .bind(include_same_repo)
        .fetch_all(pool)
        .await
    }
}

/// File pair for duplicate detection, aggregated from chunk-level similarity.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct DuplicateFilePair {
    pub file_id_a: i64,
    pub path_a: String,
    pub project_name_a: String,
    pub project_id_a: i32,
    pub file_id_b: i64,
    pub path_b: String,
    pub project_name_b: String,
    pub project_id_b: i32,
    pub language: String,
    pub avg_similarity: f64,
    pub max_similarity: f64,
    pub matching_chunks: i64,
}

/// Find duplicate file pairs across projects from the materialized table.
///
/// `include_same_repo` (default `false`) controls the worktree-aware
/// filter. When false (the common case for "find me cross-project
/// refactoring candidates"), pairs whose two projects are worktrees /
/// sibling clones of the same upstream repo are excluded. When true,
/// every cross-project pair is returned regardless of repo membership —
/// useful when an operator explicitly wants to compare branches.
///
/// Note: `cross_project_similarities` is itself populated with the
/// strict same-repo filter (see `batch_find_cross_project_neighbors`),
/// so `include_same_repo=true` here only loosens any *additional*
/// per-query filtering. Same-repo pairs that were skipped at scan time
/// won't reappear from this flag alone.
pub async fn find_duplicate_file_pairs(
    pool: &PgPool,
    min_similarity: f64,
    language: Option<&str>,
    limit: i32,
    include_same_repo: bool,
) -> Result<Vec<DuplicateFilePair>, sqlx::Error> {
    // Defense-in-depth: even though Stage 3 Q1 filters at scan time,
    // an old materialized table populated before that landed could
    // still contain same-repo pairs. The NOT EXISTS scrubs them at
    // query time. The leading `$N OR …` short-circuits when the
    // operator opts back in via `include_same_repo=true`.
    let same_repo_filter_lang = "($4 OR NOT EXISTS (
        SELECT 1 FROM projects pa, projects pb
        WHERE pa.id = s.project_id_a
          AND pb.id = s.project_id_b
          AND (
              (pa.git_common_dir IS NOT NULL AND pa.git_common_dir = pb.git_common_dir)
              OR
              (pa.git_root_commits IS NOT NULL AND pa.git_root_commits = pb.git_root_commits)
          )
    ))";
    let same_repo_filter_nolang = "($3 OR NOT EXISTS (
        SELECT 1 FROM projects pa, projects pb
        WHERE pa.id = s.project_id_a
          AND pb.id = s.project_id_b
          AND (
              (pa.git_common_dir IS NOT NULL AND pa.git_common_dir = pb.git_common_dir)
              OR
              (pa.git_root_commits IS NOT NULL AND pa.git_root_commits = pb.git_root_commits)
          )
    ))";
    if let Some(lang) = language {
        sqlx::query_as::<_, DuplicateFilePair>(&format!(
            "SELECT s.file_id_a, s.path_a, s.project_name_a, s.project_id_a,
                    s.file_id_b, s.path_b, s.project_name_b, s.project_id_b,
                    s.language,
                    AVG(s.chunk_similarity) as avg_similarity,
                    MAX(s.chunk_similarity) as max_similarity,
                    COUNT(*) as matching_chunks
             FROM cross_project_similarities s
             WHERE s.chunk_similarity >= $1
               AND s.language = $3
               AND s.project_id_a != s.project_id_b
               AND {same_repo_filter_lang}
             GROUP BY s.file_id_a, s.path_a, s.project_name_a, s.project_id_a,
                      s.file_id_b, s.path_b, s.project_name_b, s.project_id_b,
                      s.language
             HAVING AVG(s.chunk_similarity) >= $1
             ORDER BY avg_similarity DESC
             LIMIT $2",
        ))
        .bind(min_similarity)
        .bind(limit)
        .bind(lang)
        .bind(include_same_repo)
        .fetch_all(pool)
        .await
    } else {
        sqlx::query_as::<_, DuplicateFilePair>(&format!(
            "SELECT s.file_id_a, s.path_a, s.project_name_a, s.project_id_a,
                    s.file_id_b, s.path_b, s.project_name_b, s.project_id_b,
                    s.language,
                    AVG(s.chunk_similarity) as avg_similarity,
                    MAX(s.chunk_similarity) as max_similarity,
                    COUNT(*) as matching_chunks
             FROM cross_project_similarities s
             WHERE s.chunk_similarity >= $1
               AND s.project_id_a != s.project_id_b
               AND {same_repo_filter_nolang}
             GROUP BY s.file_id_a, s.path_a, s.project_name_a, s.project_id_a,
                      s.file_id_b, s.path_b, s.project_name_b, s.project_id_b,
                      s.language
             HAVING AVG(s.chunk_similarity) >= $1
             ORDER BY avg_similarity DESC
             LIMIT $2",
        ))
        .bind(min_similarity)
        .bind(limit)
        .bind(include_same_repo)
        .fetch_all(pool)
        .await
    }
}

// ============================================================================
// Chunk-level similarity (Tier 2 — DRY tools)
// ============================================================================

/// One chunk-pair from `cross_project_similarities`. Lighter-weight than
/// `DuplicateFilePair` because chunk_clusters / boilerplate_clusters /
/// pattern_abstraction_candidates aggregate at the chunk level, not file level.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ChunkSimilarityPair {
    pub chunk_id_a: i64,
    pub chunk_id_b: i64,
    pub file_id_a: i64,
    pub file_id_b: i64,
    pub path_a: String,
    pub path_b: String,
    pub project_id_a: i32,
    pub project_id_b: i32,
    pub project_name_a: String,
    pub project_name_b: String,
    pub language: String,
    pub similarity: f64,
}

/// Read chunk-pair rows from `cross_project_similarities` with the
/// usual cross-project + main-worktree + same-repo filters applied.
///
/// `main_only`: when `true`, both endpoints must belong to a project in
/// `select_main_worktree_projects` (caller passes `main_ids`). Pass an
/// empty slice to disable the main-only filter.
///
/// `project`: when `Some(name)`, at least one endpoint must belong to
/// the named project.
pub async fn find_chunk_similarity_pairs(
    pool: &PgPool,
    min_similarity: f64,
    language: Option<&str>,
    main_ids: &[i32],
    project: Option<&str>,
    include_same_repo: bool,
    limit: i32,
) -> Result<Vec<ChunkSimilarityPair>, sqlx::Error> {
    sqlx::query_as::<_, ChunkSimilarityPair>(
        "SELECT s.chunk_id_a, s.chunk_id_b,
                s.file_id_a, s.file_id_b,
                s.path_a, s.path_b,
                s.project_id_a, s.project_id_b,
                s.project_name_a, s.project_name_b,
                s.language,
                s.chunk_similarity AS similarity
         FROM cross_project_similarities s
         WHERE s.chunk_similarity >= $1
           AND s.project_id_a != s.project_id_b
           AND ($2::text IS NULL OR s.language = $2)
           AND (cardinality($3::int[]) = 0
                OR (s.project_id_a = ANY($3) AND s.project_id_b = ANY($3)))
           AND ($4::text IS NULL OR s.project_name_a = $4 OR s.project_name_b = $4)
           AND ($5 OR NOT EXISTS (
                   SELECT 1 FROM projects pa, projects pb
                   WHERE pa.id = s.project_id_a
                     AND pb.id = s.project_id_b
                     AND ((pa.git_common_dir IS NOT NULL AND pa.git_common_dir = pb.git_common_dir)
                          OR (pa.git_root_commits IS NOT NULL AND pa.git_root_commits = pb.git_root_commits))
                ))
         ORDER BY s.chunk_similarity DESC, s.chunk_id_a ASC, s.chunk_id_b ASC
         LIMIT $6",
    )
    .bind(min_similarity)
    .bind(language)
    .bind(main_ids)
    .bind(project)
    .bind(include_same_repo)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// One pair plus the topic both endpoints belong to, used by
/// `pattern_abstraction_candidates`. The endpoints sit at *medium*
/// similarity (e.g. 0.70-0.85): different implementations of the same
/// abstraction.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PatternAbstractionPair {
    pub chunk_id_a: i64,
    pub chunk_id_b: i64,
    pub file_id_a: i64,
    pub file_id_b: i64,
    pub path_a: String,
    pub path_b: String,
    pub project_id_a: i32,
    pub project_id_b: i32,
    pub project_name_a: String,
    pub project_name_b: String,
    pub language: String,
    pub similarity: f64,
    pub topic_id: i64,
    pub topic_label: String,
    pub topic_keywords: Option<Vec<String>>,
    pub membership_a: f64,
    pub membership_b: f64,
}

/// Find chunk-pairs at medium similarity (between min_sim and max_sim,
/// exclusive on the upper bound) that share the same topic. Used by
/// `pattern_abstraction_candidates` to surface candidates for trait /
/// interface extraction.
pub async fn find_pattern_abstraction_pairs(
    pool: &PgPool,
    min_similarity: f64,
    max_similarity: f64,
    min_membership: f64,
    language: Option<&str>,
    main_ids: &[i32],
    project: Option<&str>,
    include_same_repo: bool,
    limit: i32,
) -> Result<Vec<PatternAbstractionPair>, sqlx::Error> {
    sqlx::query_as::<_, PatternAbstractionPair>(
        "SELECT s.chunk_id_a, s.chunk_id_b,
                s.file_id_a, s.file_id_b,
                s.path_a, s.path_b,
                s.project_id_a, s.project_id_b,
                s.project_name_a, s.project_name_b,
                s.language,
                s.chunk_similarity AS similarity,
                cta_a.topic_id,
                ct.label AS topic_label,
                ct.keywords AS topic_keywords,
                cta_a.membership_score AS membership_a,
                cta_b.membership_score AS membership_b
         FROM cross_project_similarities s
         JOIN chunk_topic_assignments cta_a ON cta_a.chunk_id = s.chunk_id_a
         JOIN chunk_topic_assignments cta_b ON cta_b.chunk_id = s.chunk_id_b
                                            AND cta_b.topic_id = cta_a.topic_id
         JOIN code_topics ct ON ct.id = cta_a.topic_id
         WHERE s.chunk_similarity >= $1 AND s.chunk_similarity < $2
           AND s.project_id_a != s.project_id_b
           AND cta_a.membership_score >= $3
           AND cta_b.membership_score >= $3
           AND ($4::text IS NULL OR s.language = $4)
           AND (cardinality($5::int[]) = 0
                OR (s.project_id_a = ANY($5) AND s.project_id_b = ANY($5)))
           AND ($6::text IS NULL OR s.project_name_a = $6 OR s.project_name_b = $6)
           AND ($7 OR NOT EXISTS (
                   SELECT 1 FROM projects pa, projects pb
                   WHERE pa.id = s.project_id_a
                     AND pb.id = s.project_id_b
                     AND ((pa.git_common_dir IS NOT NULL AND pa.git_common_dir = pb.git_common_dir)
                          OR (pa.git_root_commits IS NOT NULL AND pa.git_root_commits = pb.git_root_commits))
                ))
         ORDER BY cta_a.topic_id, s.chunk_similarity DESC, s.chunk_id_a ASC, s.chunk_id_b ASC
         LIMIT $8",
    )
    .bind(min_similarity)
    .bind(max_similarity)
    .bind(min_membership)
    .bind(language)
    .bind(main_ids)
    .bind(project)
    .bind(include_same_repo)
    .bind(limit)
    .fetch_all(pool)
    .await
}
