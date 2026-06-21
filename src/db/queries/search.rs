//! Search readers: dense semantic, hybrid RRF, full-text, grep, prompt
//! recall, and mandate FTS. Extracted from `queries.rs` (god-file split).
#![allow(unused_imports)]

use crate::db::queries::*;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

// ============================================================================
// Search queries
// ============================================================================

/// Serialize a similarity score rounded to 4 decimal places. Cosine scores carry
/// ~16 significant digits off pgvector, but 4 decimals is ample for ranking and
/// trims ~12 bytes/hit. Serialization-only — the `FromRow` decode is unaffected.
fn serialize_rounded_score<S>(score: &Option<f64>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match score {
        Some(v) => serializer.serialize_f64((v * 1e4).round() / 1e4),
        None => serializer.serialize_none(),
    }
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct SearchResult {
    pub path: String,
    pub relative_path: String,
    pub language: String,
    pub chunk_content: String,
    pub start_line: i32,
    pub end_line: i32,
    #[serde(serialize_with = "serialize_rounded_score")]
    pub score: Option<f64>,
    pub project_name: String,
    /// Chunk id, surfaced by `hybrid_search_chunks` so the API handler can fetch
    /// per-candidate features (embedding, blame_date) for MMR diversity +
    /// recency re-ranking (Phase 4.2). `#[sqlx(default)]` ⇒ other SearchResult
    /// queries (semantic_search) that don't select it default to None.
    #[sqlx(default)]
    pub chunk_id: Option<i64>,
}

/// Semantic search using vector similarity.
///
/// Sets `hnsw.ef_search` on the connection for improved recall before executing
/// the k-NN query. Supports optional filtering by language and/or project name.
///
/// `dedupe_worktrees=true` collapses cross-worktree duplicates of the
/// same `(repo, relative_path)` to a single canonical (lowest-id-project)
/// hit. `dedupe_worktrees=false` (the default) preserves every hit.
pub async fn semantic_search(
    pool: &PgPool,
    embedding: &[f32],
    limit: i32,
    language: Option<&str>,
    project: Option<&str>,
    ef_search: i32,
    dedupe_worktrees: bool,
) -> Result<Vec<SearchResult>, sqlx::Error> {
    let embedding_vec = pgvector::Vector::from(embedding.to_vec());

    // BGE-M3-only: the query runs against the 1024-d `embedding_v2`
    // column. A non-1024 query embedding is a configuration error and
    // surfaces a clear protocol error.
    if embedding.len() != 1024 {
        return Err(sqlx::Error::Protocol(format!(
            "semantic_search: expected a 1024-dimension BGE-M3 query embedding, got {}",
            embedding.len()
        )));
    }
    let col = "embedding_v2";

    // Acquire a dedicated connection so ef_search applies to our query.
    // Using SET LOCAL within a transaction keeps it scoped to this operation.
    let mut tx = pool.begin().await?;

    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SET LOCAL hnsw.ef_search = {}",
        ef_search
    )))
    .execute(&mut *tx)
    .await?;

    // Build the query dynamically based on which filters are present.
    // The dedup clause's `$N` index is determined by how many other
    // params come before it in the bind order.
    let results = match (language, project) {
        (Some(lang), Some(proj)) => {
            // $1=embedding, $2=limit, $3=lang, $4=proj, $5=dedupe
            sqlx::query_as::<_, SearchResult>(sqlx::AssertSqlSafe(format!(
                "SELECT f.path, f.relative_path, f.language,
                        c.content as chunk_content, c.start_line, c.end_line,
                        1 - (c.{col} <=> $1) as score,
                        p.name as project_name
                 FROM file_chunks c
                 JOIN indexed_files f ON f.id = c.file_id
                 JOIN projects p ON p.id = f.project_id
                 WHERE f.language = $3 AND p.name = $4
                   AND c.{col} IS NOT NULL
                   AND {dedup}
                 ORDER BY c.{col} <=> $1
                 LIMIT $2",
                col = col,
                dedup = worktree_dedup_clause(5)
            )))
            .bind(&embedding_vec)
            .bind(limit)
            .bind(lang)
            .bind(proj)
            .bind(dedupe_worktrees)
            .fetch_all(&mut *tx)
            .await?
        }
        (Some(lang), None) => {
            // $1=embedding, $2=limit, $3=lang, $4=dedupe
            sqlx::query_as::<_, SearchResult>(sqlx::AssertSqlSafe(format!(
                "SELECT f.path, f.relative_path, f.language,
                        c.content as chunk_content, c.start_line, c.end_line,
                        1 - (c.{col} <=> $1) as score,
                        p.name as project_name
                 FROM file_chunks c
                 JOIN indexed_files f ON f.id = c.file_id
                 JOIN projects p ON p.id = f.project_id
                 WHERE f.language = $3
                   AND c.{col} IS NOT NULL
                   AND {dedup}
                 ORDER BY c.{col} <=> $1
                 LIMIT $2",
                col = col,
                dedup = worktree_dedup_clause(4)
            )))
            .bind(&embedding_vec)
            .bind(limit)
            .bind(lang)
            .bind(dedupe_worktrees)
            .fetch_all(&mut *tx)
            .await?
        }
        (None, Some(proj)) => {
            // $1=embedding, $2=limit, $3=proj, $4=dedupe
            sqlx::query_as::<_, SearchResult>(sqlx::AssertSqlSafe(format!(
                "SELECT f.path, f.relative_path, f.language,
                        c.content as chunk_content, c.start_line, c.end_line,
                        1 - (c.{col} <=> $1) as score,
                        p.name as project_name
                 FROM file_chunks c
                 JOIN indexed_files f ON f.id = c.file_id
                 JOIN projects p ON p.id = f.project_id
                 WHERE p.name = $3
                   AND c.{col} IS NOT NULL
                   AND {dedup}
                 ORDER BY c.{col} <=> $1
                 LIMIT $2",
                col = col,
                dedup = worktree_dedup_clause(4)
            )))
            .bind(&embedding_vec)
            .bind(limit)
            .bind(proj)
            .bind(dedupe_worktrees)
            .fetch_all(&mut *tx)
            .await?
        }
        (None, None) => {
            // $1=embedding, $2=limit, $3=dedupe
            sqlx::query_as::<_, SearchResult>(sqlx::AssertSqlSafe(format!(
                "SELECT f.path, f.relative_path, f.language,
                        c.content as chunk_content, c.start_line, c.end_line,
                        1 - (c.{col} <=> $1) as score,
                        p.name as project_name
                 FROM file_chunks c
                 JOIN indexed_files f ON f.id = c.file_id
                 JOIN projects p ON p.id = f.project_id
                 WHERE c.{col} IS NOT NULL
                   AND {dedup}
                 ORDER BY c.{col} <=> $1
                 LIMIT $2",
                col = col,
                dedup = worktree_dedup_clause(3)
            )))
            .bind(&embedding_vec)
            .bind(limit)
            .bind(dedupe_worktrees)
            .fetch_all(&mut *tx)
            .await?
        }
    };

    tx.commit().await?;

    Ok(results)
}

/// Build the `hybrid_search_chunks` SQL string. Pure (no DB, no I/O) so a no-DB
/// unit test can assert the fused RRF column is cast to `float8` in BOTH the
/// 2-leg (`with_sparse=false`) and 3-leg (`with_sparse=true`) branches. The
/// fused RRF sum `Σ COALESCE(1.0/(60.0+rnk), 0.0)` is typed NUMERIC by Postgres;
/// `SearchResult.score: Option<f64>` decodes FLOAT8, so the sum MUST be wrapped
/// `( … )::float8`. Removing either the parens or the cast reintroduces the
/// `/api/search` HTTP-500 decode bug — guarded by `hybrid_search_sql_tests`.
pub(crate) fn hybrid_search_sql(col: &str, with_sparse: bool) -> String {
    // $1=embedding, $2=query_text, $3=candidates, $4=language, $5=project, $6=limit, $7=sparse
    let dense_lexical = format!(
        "dense AS (
            SELECT chunk_id, ROW_NUMBER() OVER (ORDER BY dist) AS rnk FROM (
                SELECT c.id AS chunk_id, (c.{col} <=> $1) AS dist
                FROM file_chunks c
                JOIN indexed_files f ON f.id = c.file_id
                JOIN projects p ON p.id = f.project_id
                WHERE c.{col} IS NOT NULL
                  AND ($4::text IS NULL OR f.language = $4)
                  AND ($5::text IS NULL OR p.name = $5)
                ORDER BY c.{col} <=> $1
                LIMIT $3
            ) dd
        ),
        lexical AS (
            SELECT chunk_id, ROW_NUMBER() OVER (ORDER BY rank DESC) AS rnk FROM (
                SELECT c.id AS chunk_id,
                       ts_rank(c.content_tsv, plainto_tsquery('english', $2)) AS rank
                FROM file_chunks c
                JOIN indexed_files f ON f.id = c.file_id
                JOIN projects p ON p.id = f.project_id
                WHERE c.content_tsv @@ plainto_tsquery('english', $2)
                  AND ($4::text IS NULL OR f.language = $4)
                  AND ($5::text IS NULL OR p.name = $5)
                ORDER BY rank DESC
                LIMIT $3
            ) ll
        )",
        col = col
    );
    let select_tail = "SELECT f.path, f.relative_path, f.language,
               c.content AS chunk_content, c.start_line, c.end_line,
               fused.rrf AS score,
               p.name AS project_name,
               c.id AS chunk_id
        FROM fused
        JOIN file_chunks c ON c.id = fused.chunk_id
        JOIN indexed_files f ON f.id = c.file_id
        JOIN projects p ON p.id = f.project_id
        ORDER BY fused.rrf DESC
        LIMIT $6";

    if with_sparse {
        format!(
            "WITH {dense_lexical},
            sparse AS (
                SELECT chunk_id, ROW_NUMBER() OVER (ORDER BY dist) AS rnk FROM (
                    SELECT c.id AS chunk_id, (c.sparse_v2 <#> $7) AS dist
                    FROM file_chunks c
                    JOIN indexed_files f ON f.id = c.file_id
                    JOIN projects p ON p.id = f.project_id
                    WHERE c.sparse_v2 IS NOT NULL
                      AND ($4::text IS NULL OR f.language = $4)
                      AND ($5::text IS NULL OR p.name = $5)
                    ORDER BY c.sparse_v2 <#> $7
                    LIMIT $3
                ) ss
            ),
            fused AS (
                SELECT COALESCE(d.chunk_id, l.chunk_id, s.chunk_id) AS chunk_id,
                       (COALESCE(1.0 / (60.0 + d.rnk), 0.0)
                     + COALESCE(1.0 / (60.0 + l.rnk), 0.0)
                     + COALESCE(1.0 / (60.0 + s.rnk), 0.0))::float8 AS rrf
                FROM dense d
                FULL OUTER JOIN lexical l ON d.chunk_id = l.chunk_id
                FULL OUTER JOIN sparse s ON COALESCE(d.chunk_id, l.chunk_id) = s.chunk_id
            )
            {select_tail}"
        )
    } else {
        format!(
            "WITH {dense_lexical},
            fused AS (
                SELECT COALESCE(d.chunk_id, l.chunk_id) AS chunk_id,
                       (COALESCE(1.0 / (60.0 + d.rnk), 0.0)
                     + COALESCE(1.0 / (60.0 + l.rnk), 0.0))::float8 AS rrf
                FROM dense d
                FULL OUTER JOIN lexical l ON d.chunk_id = l.chunk_id
            )
            {select_tail}"
        )
    }
}

/// Chunk-level hybrid search: dense ANN + BM25 full-text, fused by Reciprocal
/// Rank Fusion (Cormack et al. 2009) entirely in SQL, with optional language /
/// project filters applied to both legs. Returns chunk-level `SearchResult`s
/// ordered by RRF score (carried in `score`). Backs the `/api/search` hook,
/// which then optionally cross-encoder-reranks the top results.
///
/// `candidates` bounds each leg's contribution (per-leg `LIMIT`); `limit` is
/// the fused output size. Uses the same `SET LOCAL hnsw.ef_search` discipline
/// as `semantic_search` so the ANN leg honours the configured recall budget.
#[allow(clippy::too_many_arguments)]
pub async fn hybrid_search_chunks(
    pool: &PgPool,
    query_text: &str,
    embedding: &[f32],
    limit: i32,
    candidates: i32,
    language: Option<&str>,
    project: Option<&str>,
    ef_search: i32,
    query_sparse: Option<&pgvector::SparseVector>,
) -> Result<Vec<SearchResult>, sqlx::Error> {
    if embedding.len() != 1024 {
        return Err(sqlx::Error::Protocol(format!(
            "hybrid_search_chunks: expected a 1024-dimension BGE-M3 query embedding, got {}",
            embedding.len()
        )));
    }
    let col = "embedding_v2";
    let embedding_vec = pgvector::Vector::from(embedding.to_vec());

    let mut tx = pool.begin().await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SET LOCAL hnsw.ef_search = {}",
        ef_search
    )))
    .execute(&mut *tx)
    .await?;

    // Optional filters collapse into one query via `($n IS NULL OR …)`.
    // SQL is built by the pure `hybrid_search_sql` helper so a no-DB unit test
    // can pin the `::float8` cast on the fused RRF column (see that fn's docs).
    // $1=embedding, $2=query_text, $3=candidates, $4=language, $5=project, $6=limit, $7=sparse
    let results = if let Some(sparse) = query_sparse {
        let sql = hybrid_search_sql(col, true);
        sqlx::query_as::<_, SearchResult>(sqlx::AssertSqlSafe(sql.as_str()))
            .bind(&embedding_vec)
            .bind(query_text)
            .bind(candidates)
            .bind(language)
            .bind(project)
            .bind(limit)
            .bind(sparse)
            .fetch_all(&mut *tx)
            .await?
    } else {
        let sql = hybrid_search_sql(col, false);
        sqlx::query_as::<_, SearchResult>(sqlx::AssertSqlSafe(sql.as_str()))
            .bind(&embedding_vec)
            .bind(query_text)
            .bind(candidates)
            .bind(language)
            .bind(project)
            .bind(limit)
            .fetch_all(&mut *tx)
            .await?
    };

    tx.commit().await?;
    Ok(results)
}

/// Full-text search using PostgreSQL tsvector/tsquery over per-chunk
/// FTS. Returns one row per matching file with `content` set to the
/// best-ranked chunk's body (not the whole file); this preserves the
/// previous result shape while supporting plain-text files whose
/// `indexed_files.content` is `NULL` (asymmetric-storage policy —
/// see `upsert_file`).
///
/// `dedupe_worktrees=true` collapses cross-worktree duplicates of the
/// same `(repo, relative_path)` to a single canonical hit. See the
/// `worktree_dedup_clause` helper for the filter shape.
/// Core BM25/full-text query, generic over the sqlx executor so the pooled
/// [`text_search`] and the transaction-scoped [`text_search_bounded`] share a
/// single source of truth for the SQL (no drift). Ranks via the stored
/// `file_chunks.content_tsv` generated column (v13 migration) so the heavy
/// `to_tsvector` is precomputed at write time rather than recomputed per row
/// on every query — this is what keeps the BM25 leg fast under load.
async fn run_text_search_query<'e, E>(
    executor: E,
    query: &str,
    limit: i32,
    language: Option<&str>,
    project: Option<&str>,
    dedupe_worktrees: bool,
) -> Result<Vec<TextSearchResult>, sqlx::Error>
where
    E: sqlx::PgExecutor<'e>,
{
    // Strategy: rank every chunk that matches, then DISTINCT ON file_id
    // keeping the top-ranked chunk per file. `ORDER BY file_id, rank
    // DESC` lets DISTINCT ON pick the best chunk per file; the outer
    // SELECT re-sorts by rank globally and applies the limit. Chunks
    // hang off `COALESCE(duplicate_of_file_id, id)` so duplicates point
    // at canonical chunks.
    let results = sqlx::query_as::<_, TextSearchResult>(sqlx::AssertSqlSafe(format!(
        "SELECT path, relative_path, language, content, rank FROM (
            SELECT DISTINCT ON (f.id)
                f.path,
                f.relative_path,
                f.language,
                c.content,
                ts_rank(c.content_tsv, plainto_tsquery('english', $1)) AS rank
            FROM file_chunks c
            JOIN indexed_files f ON c.file_id = COALESCE(f.duplicate_of_file_id, f.id)
            LEFT JOIN projects p ON p.id = f.project_id
            WHERE c.content_tsv @@ plainto_tsquery('english', $1)
              AND ($3::text IS NULL OR f.language = $3)
              AND ($4::text IS NULL OR p.name = $4)
              AND {}
            ORDER BY f.id, rank DESC
         ) per_file
         ORDER BY rank DESC
         LIMIT $2",
        worktree_dedup_clause(5)
    )))
    .bind(query)
    .bind(limit)
    .bind(language)
    .bind(project)
    .bind(dedupe_worktrees)
    .fetch_all(executor)
    .await?;

    Ok(results)
}

pub async fn text_search(
    pool: &PgPool,
    query: &str,
    limit: i32,
    language: Option<&str>,
    project: Option<&str>,
    dedupe_worktrees: bool,
) -> Result<Vec<TextSearchResult>, sqlx::Error> {
    run_text_search_query(pool, query, limit, language, project, dedupe_worktrees).await
}

/// Same as [`text_search`], but caps the query with a per-call
/// `SET LOCAL statement_timeout` scoped to an explicit transaction. A cold or
/// write-contended GIN index can otherwise exceed the daemon-wide 30s ceiling;
/// the tighter bound lets the caller (`hybrid_search`'s text leg) give up fast
/// and degrade instead of failing. `SET LOCAL` reverts at commit/rollback, so
/// it never leaks onto the pooled connection.
pub async fn text_search_bounded(
    pool: &PgPool,
    query: &str,
    limit: i32,
    language: Option<&str>,
    project: Option<&str>,
    dedupe_worktrees: bool,
    statement_timeout_ms: u32,
) -> Result<Vec<TextSearchResult>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    // `statement_timeout_ms` is a u32 → digits only, no injection surface.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SET LOCAL statement_timeout = {statement_timeout_ms}"
    )))
    .execute(&mut *tx)
    .await?;
    let results =
        run_text_search_query(&mut *tx, query, limit, language, project, dedupe_worktrees).await?;
    tx.commit().await?;
    Ok(results)
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct TextSearchResult {
    pub path: String,
    pub relative_path: String,
    pub language: String,
    pub content: Option<String>,
    pub rank: Option<f32>,
}

/// Regex grep search across file contents.
///
/// `dedupe_worktrees=true` collapses cross-worktree duplicates of the
/// same `(repo, relative_path)` to a single canonical hit. See the
/// `worktree_dedup_clause` helper for the filter shape.
/// Regex grep across per-chunk content. Returns one row per matching
/// file with `content` set to the first matching chunk's body (not the
/// whole file); plain-text files whose `indexed_files.content` is NULL
/// remain searchable because chunks always carry the text.
///
/// `dedupe_worktrees=true` collapses cross-worktree duplicates of the
/// same `(repo, relative_path)` to a single canonical hit. See
/// `worktree_dedup_clause` for the filter shape.
/// Translate the PCRE/`rg`/GNU word-boundary escapes that PostgreSQL's POSIX
/// Advanced Regular Expression engine spells differently, leaving every other
/// escape untouched.
///
/// PG ARE reads `\b` as a **literal backspace** (a character-entry escape), not
/// a word boundary — so a `\bword\b` pattern matches nothing in source text,
/// while `rg`/GNU grep treat `\b` as a zero-width boundary and match. PG's
/// boundary escapes are `\y` (boundary), `\Y` (non-boundary), `\m` (start of
/// word), `\M` (end of word). We map `\b`→`\y`, `\B`→`\Y`, and the GNU
/// `\<`/`\>`→`\m`/`\M`. Class shorthands (`\d`/`\w`/`\s`/…) already work in PG
/// ARE and are left alone. A `\b` **inside** a bracket expression (`[...]`)
/// legitimately means backspace in both engines, so bracket interiors are
/// skipped — only word-boundary escapes outside a character class are rewritten.
fn translate_pcre_boundaries(pattern: &str) -> std::borrow::Cow<'_, str> {
    if !pattern.contains('\\') {
        return std::borrow::Cow::Borrowed(pattern);
    }
    let mut out = String::with_capacity(pattern.len() + 4);
    let mut in_class = false; // inside a [...] character class
    let mut changed = false;
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&n) = chars.peek() {
                let repl = if in_class {
                    None
                } else {
                    match n {
                        'b' => Some('y'),
                        'B' => Some('Y'),
                        '<' => Some('m'),
                        '>' => Some('M'),
                        _ => None,
                    }
                };
                out.push('\\');
                match repl {
                    Some(r) => {
                        out.push(r);
                        changed = true;
                    }
                    // Copy the escaped char verbatim (incl. `\b` in a class,
                    // `\d`, `\\`, `\[`) so it can never toggle `in_class`.
                    None => out.push(n),
                }
                chars.next();
            } else {
                out.push('\\'); // trailing backslash
            }
            continue;
        }
        match c {
            '[' => in_class = true,
            ']' => in_class = false,
            _ => {}
        }
        out.push(c);
    }
    if changed {
        std::borrow::Cow::Owned(out)
    } else {
        std::borrow::Cow::Borrowed(pattern)
    }
}

pub async fn grep_search(
    pool: &PgPool,
    pattern: &str,
    glob: Option<&str>,
    limit: i32,
    dedupe_worktrees: bool,
) -> Result<Vec<GrepResult>, sqlx::Error> {
    let pat = translate_pcre_boundaries(pattern);
    let results = if let Some(glob_pattern) = glob {
        // Convert glob to SQL LIKE pattern.
        // $1=pattern, $2=limit, $3=like, $4=dedupe
        let like_pattern = glob_pattern.replace('*', "%").replace('?', "_");
        sqlx::query_as::<_, GrepResult>(sqlx::AssertSqlSafe(format!(
            "SELECT DISTINCT ON (f.id)
                f.path,
                f.relative_path,
                f.language,
                c.content
             FROM file_chunks c
             JOIN indexed_files f ON c.file_id = COALESCE(f.duplicate_of_file_id, f.id)
             WHERE c.content ~ $1
               AND f.relative_path LIKE $3
               AND {}
             ORDER BY f.id, c.chunk_index
             LIMIT $2",
            worktree_dedup_clause(4)
        )))
        .bind(pat.as_ref())
        .bind(limit)
        .bind(&like_pattern)
        .bind(dedupe_worktrees)
        .fetch_all(pool)
        .await?
    } else {
        // $1=pattern, $2=limit, $3=dedupe
        sqlx::query_as::<_, GrepResult>(sqlx::AssertSqlSafe(format!(
            "SELECT DISTINCT ON (f.id)
                f.path,
                f.relative_path,
                f.language,
                c.content
             FROM file_chunks c
             JOIN indexed_files f ON c.file_id = COALESCE(f.duplicate_of_file_id, f.id)
             WHERE c.content ~ $1
               AND {}
             ORDER BY f.id, c.chunk_index
             LIMIT $2",
            worktree_dedup_clause(3)
        )))
        .bind(pat.as_ref())
        .bind(limit)
        .bind(dedupe_worktrees)
        .fetch_all(pool)
        .await?
    };

    Ok(results)
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct GrepResult {
    pub path: String,
    pub relative_path: String,
    pub language: String,
    pub content: Option<String>,
}

// ============================================================================
// Memory-server Phase 0 queries
// ============================================================================
//
// `recall_prompts_semantic` exposes the already-embedded `session_prompts`
// archive via vector similarity. The column has been populated on every
// prompt since the session-mandates feature shipped but had zero readers
// before this; surfacing it as an MCP tool is the cheapest possible
// memory-server feature (no schema change, HNSW index already exists).
//
// `search_mandates_fts` adds a search surface to `durable_mandates`, which
// previously had a single reader (project-scope dump). Postgres full-text
// over `imperative || ' ' || target` is the Phase 0 mode; semantic search
// adds a `durable_mandates.embedding` column after Phase 1 cutover (the
// 1024d BGE-M3 column).

/// Vector-similarity search over `session_prompts`. Returns the top-k most
/// similar historical prompts, reading from the 1024-d BGE-M3
/// `embedding_v2` column.
///
/// `project_name` and `session_id` are independent filters; both may be
/// `None`. `limit` is clamped to [1, 200] by the caller.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct PromptRecallResult {
    pub id: i64,
    pub session_id: uuid::Uuid,
    pub project_name: Option<String>,
    pub ts: DateTime<Utc>,
    pub prompt_text: String,
    pub similarity: Option<f64>,
}

pub async fn recall_prompts_semantic(
    pool: &PgPool,
    embedding: &[f32],
    project_name: Option<&str>,
    session_id: Option<uuid::Uuid>,
    limit: i32,
    ef_search: i32,
) -> Result<Vec<PromptRecallResult>, sqlx::Error> {
    let embedding_vec = pgvector::Vector::from(embedding.to_vec());

    // BGE-M3-only: read from the 1024-d `embedding_v2` column. A
    // non-1024 query embedding is rejected here so an accidental
    // misconfiguration surfaces as a clear error instead of as
    // wrong-shape vector arithmetic at the pgvector layer.
    if embedding.len() != 1024 {
        return Err(sqlx::Error::Protocol(format!(
            "recall_prompts: expected a 1024-dimension BGE-M3 query embedding, got {}",
            embedding.len()
        )));
    }
    let column = "embedding_v2";

    let mut tx = pool.begin().await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SET LOCAL hnsw.ef_search = {}",
        ef_search
    )))
    .execute(&mut *tx)
    .await?;

    // Column-name interpolation is safe — it's chosen from a closed
    // whitelist above, not from user input.
    let sql = format!(
        "SELECT sp.id,
                sp.session_id,
                p.name AS project_name,
                sp.ts,
                sp.prompt_text,
                1 - (sp.{col} <=> $1) AS similarity
         FROM session_prompts sp
         JOIN sessions s ON s.id = sp.session_id
         LEFT JOIN projects p ON p.id = s.project_id
         WHERE sp.{col} IS NOT NULL
           AND ($2::text IS NULL OR p.name = $2)
           AND ($3::uuid IS NULL OR sp.session_id = $3)
         ORDER BY sp.{col} <=> $1
         LIMIT $4",
        col = column,
    );

    let rows = sqlx::query_as::<_, PromptRecallResult>(sqlx::AssertSqlSafe(sql.as_str()))
        .bind(&embedding_vec)
        .bind(project_name)
        .bind(session_id)
        .bind(limit.clamp(1, 200))
        .fetch_all(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(rows)
}

/// Full-text search over `durable_mandates`. Phase 0 surface — adds a
/// semantic mode after Phase 1 cutover provisions a 1024d embedding
/// column.
///
/// `query_text` is matched against `imperative || ' ' || COALESCE(target,'')`
/// using `plainto_tsquery('english', $1)`. `polarity` and `scope` are
/// optional exact-match filters.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct MandateSearchResult {
    pub id: i64,
    pub scope: String,
    pub project_id: Option<i32>,
    pub project_name: Option<String>,
    pub polarity: String,
    pub imperative: String,
    pub target: Option<String>,
    pub promoted_at: DateTime<Utc>,
    pub file_path: Option<String>,
    pub rank: Option<f32>,
}

pub async fn search_mandates_fts(
    pool: &PgPool,
    query_text: &str,
    polarity: Option<&str>,
    scope: Option<&str>,
    project_id: Option<i32>,
    limit: i32,
) -> Result<Vec<MandateSearchResult>, sqlx::Error> {
    sqlx::query_as::<_, MandateSearchResult>(
        "SELECT m.id, m.scope, m.project_id, p.name AS project_name,
                m.polarity, m.imperative, m.target, m.promoted_at, m.file_path,
                ts_rank_cd(
                  to_tsvector('english', m.imperative || ' ' || COALESCE(m.target, '')),
                  plainto_tsquery('english', $1)
                ) AS rank
         FROM durable_mandates m
         LEFT JOIN projects p ON p.id = m.project_id
         WHERE to_tsvector('english', m.imperative || ' ' || COALESCE(m.target, ''))
               @@ plainto_tsquery('english', $1)
           AND ($2::text IS NULL OR m.polarity = $2)
           AND ($3::text IS NULL OR m.scope = $3)
           AND ($4::int  IS NULL OR m.project_id = $4 OR m.scope = 'workspace')
         ORDER BY rank DESC NULLS LAST, m.promoted_at DESC
         LIMIT $5",
    )
    .bind(query_text)
    .bind(polarity)
    .bind(scope)
    .bind(project_id)
    .bind(limit.clamp(1, 200))
    .fetch_all(pool)
    .await
}

/// Vector-similarity search over `durable_mandates` — the v31 semantic leg of
/// `search_mandates`. Same `polarity`/`scope`/`project_id` filters as
/// `search_mandates_fts`; `rank` carries the cosine similarity (`1 - distance`,
/// cast to `float4` to dodge the NUMERIC-decode trap) instead of the FTS rank.
/// Reads the 1024-d BGE-M3 `embedding` column; a non-1024 query embedding is
/// rejected so a misconfiguration surfaces clearly rather than as wrong-shape
/// vector arithmetic.
pub async fn search_mandates_semantic(
    pool: &PgPool,
    embedding: &[f32],
    polarity: Option<&str>,
    scope: Option<&str>,
    project_id: Option<i32>,
    limit: i32,
    ef_search: i32,
) -> Result<Vec<MandateSearchResult>, sqlx::Error> {
    if embedding.len() != 1024 {
        return Err(sqlx::Error::Protocol(format!(
            "search_mandates: expected a 1024-dimension BGE-M3 query embedding, got {}",
            embedding.len()
        )));
    }
    let embedding_vec = pgvector::Vector::from(embedding.to_vec());
    let mut tx = pool.begin().await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SET LOCAL hnsw.ef_search = {}",
        ef_search
    )))
    .execute(&mut *tx)
    .await?;
    let rows = sqlx::query_as::<_, MandateSearchResult>(
        "SELECT m.id, m.scope, m.project_id, p.name AS project_name,
                m.polarity, m.imperative, m.target, m.promoted_at, m.file_path,
                (1.0 - (m.embedding <=> $1))::float4 AS rank
         FROM durable_mandates m
         LEFT JOIN projects p ON p.id = m.project_id
         WHERE m.embedding IS NOT NULL
           AND ($2::text IS NULL OR m.polarity = $2)
           AND ($3::text IS NULL OR m.scope = $3)
           AND ($4::int  IS NULL OR m.project_id = $4 OR m.scope = 'workspace')
         ORDER BY m.embedding <=> $1
         LIMIT $5",
    )
    .bind(&embedding_vec)
    .bind(polarity)
    .bind(scope)
    .bind(project_id)
    .bind(limit.clamp(1, 200))
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows)
}

/// Chunk-anchored grep match. Returned by the per-chunk variant of
/// `grep_search`. The chunk metadata lets the tool body (or the agent)
/// expand context lines on demand without re-querying the full file.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct GrepChunkResult {
    pub project_name: String,
    pub path: String,
    pub relative_path: String,
    pub language: String,
    pub chunk_index: i32,
    pub start_line: i32,
    pub end_line: i32,
    pub content: String,
}

/// Per-chunk grep: returns the matching `file_chunks` rows for `pattern`,
/// optionally filtered by project name / language / glob and with case
/// insensitivity. Unlike `grep_search` (which returns whole files), this
/// helper anchors each match to a specific `(chunk_index, start_line,
/// end_line)` so the caller can return a small slice — typically ~500
/// tokens per hit instead of an entire file's worth.
pub async fn grep_search_chunks(
    pool: &PgPool,
    pattern: &str,
    project: Option<&str>,
    language: Option<&str>,
    glob: Option<&str>,
    case_insensitive: bool,
    limit: i32,
    dedupe_worktrees: bool,
) -> Result<Vec<GrepChunkResult>, sqlx::Error> {
    let regex_op = if case_insensitive { "~*" } else { "~" };
    let like_pattern = glob.map(|g| g.replace('*', "%").replace('?', "_"));
    // Param layout: $1=pattern, $2=project, $3=language, $4=like (or NULL),
    // $5=dedupe, $6=limit.
    let sql = format!(
        "SELECT
            p.name AS project_name,
            f.path,
            f.relative_path,
            f.language,
            c.chunk_index,
            c.start_line,
            c.end_line,
            c.content
         FROM file_chunks c
         JOIN indexed_files f ON c.file_id = COALESCE(f.duplicate_of_file_id, f.id)
         JOIN projects p ON p.id = f.project_id
         WHERE c.content {regex_op} $1
           AND ($2::text IS NULL OR p.name = $2)
           AND ($3::text IS NULL OR f.language = $3)
           AND ($4::text IS NULL OR f.relative_path LIKE $4)
           AND {dedup}
         ORDER BY f.path, c.chunk_index
         LIMIT $6",
        regex_op = regex_op,
        dedup = worktree_dedup_clause(5),
    );

    let pat = translate_pcre_boundaries(pattern);
    let rows = sqlx::query_as::<_, GrepChunkResult>(sqlx::AssertSqlSafe(sql.as_str()))
        .bind(pat.as_ref())
        .bind(project)
        .bind(language)
        .bind(like_pattern)
        .bind(dedupe_worktrees)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

#[cfg(test)]
mod boundary_translation_tests {
    use super::translate_pcre_boundaries;

    #[test]
    fn maps_word_boundaries_outside_brackets() {
        assert_eq!(translate_pcre_boundaries(r"\bword\b"), r"\yword\y");
        assert_eq!(translate_pcre_boundaries(r"\Bword"), r"\Yword");
        assert_eq!(translate_pcre_boundaries(r"\<id\>"), r"\mid\M");
    }

    #[test]
    fn leaves_class_shorthands_and_bracket_backspace_untouched() {
        // Class shorthands already work in PG ARE.
        assert_eq!(translate_pcre_boundaries(r"\d+\s\w*"), r"\d+\s\w*");
        // `\b` inside [...] is a legitimate backspace in both engines.
        assert_eq!(translate_pcre_boundaries(r"[\b]"), r"[\b]");
        assert_eq!(
            translate_pcre_boundaries(r"foo[a\bc]\bbar"),
            r"foo[a\bc]\ybar"
        );
    }

    #[test]
    fn no_backslash_is_borrowed_unchanged() {
        let p = "plain_text_pattern";
        assert!(matches!(
            translate_pcre_boundaries(p),
            std::borrow::Cow::Borrowed(_)
        ));
        // A non-boundary escape leaves the string logically unchanged.
        assert_eq!(translate_pcre_boundaries(r"a\.b"), r"a\.b");
    }

    #[test]
    fn handles_trailing_backslash_without_panic() {
        assert_eq!(translate_pcre_boundaries(r"abc\"), "abc\\");
    }
}

#[cfg(test)]
mod hybrid_search_sql_tests {
    use super::hybrid_search_sql;

    // No DB, no async: runs under verify.sh Gate 4 (`cargo test --release --bin
    // pgmcp`) and Gate 8. Asserting the contiguous `))::float8 AS rrf` pins BOTH
    // the wrapping parens and the cast: dropping the cast yields `0.0) AS rrf`,
    // and dropping the parens yields `0.0)::float8 AS rrf` (which, by `::`
    // precedence, casts only the last term — the sum stays NUMERIC). Either
    // regression reintroduces the /api/search HTTP-500 decode bug.
    const CAST: &str = "))::float8 AS rrf";

    #[test]
    fn two_leg_branch_casts_fused_rrf_to_float8() {
        let sql = hybrid_search_sql("embedding_v2", false);
        assert!(
            sql.contains(CAST),
            "2-leg fused RRF must be wrapped+cast:\n{sql}"
        );
        assert!(
            sql.contains("fused.rrf AS score"),
            "missing score projection:\n{sql}"
        );
        assert!(
            sql.contains("ORDER BY fused.rrf DESC"),
            "missing ordering:\n{sql}"
        );
        assert!(
            !sql.contains("sparse_v2"),
            "2-leg must not pull the sparse leg:\n{sql}"
        );
        assert!(
            sql.contains("c.embedding_v2 <=> $1"),
            "dense leg must use col:\n{sql}"
        );
    }

    #[test]
    fn three_leg_branch_casts_fused_rrf_to_float8() {
        let sql = hybrid_search_sql("embedding_v2", true);
        assert!(
            sql.contains(CAST),
            "3-leg fused RRF must be wrapped+cast:\n{sql}"
        );
        assert!(
            sql.contains("fused.rrf AS score"),
            "missing score projection:\n{sql}"
        );
        assert!(
            sql.contains("ORDER BY fused.rrf DESC"),
            "missing ordering:\n{sql}"
        );
        assert!(
            sql.contains("c.sparse_v2 <#> $7"),
            "3-leg must include the sparse leg:\n{sql}"
        );
        assert!(
            sql.contains("60.0 + s.rnk"),
            "3-leg must add the sparse RRF term:\n{sql}"
        );
    }
}
