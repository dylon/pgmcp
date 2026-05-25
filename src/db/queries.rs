//! Database query functions.

#[path = "queries/stats.rs"]
mod queries_stats;
pub use queries_stats::*;

use chrono::{DateTime, Utc};
use sqlx::PgPool;

// ============================================================================
// Project queries
// ============================================================================

/// Upsert a project (create or update).
///
/// `git_common_dir` and `git_root_commits` are the worktree-grouping
/// signals (see `src/indexer/git_indexer.rs::detect_git_common_dir` /
/// `detect_git_root_commits`). Both are optional and pass `None` for
/// non-git projects. Re-scans update the values via the ON CONFLICT
/// branch so adding/removing/cloning a worktree is reflected on the
/// next scan.
pub async fn upsert_project(
    pool: &PgPool,
    workspace_path: &str,
    path: &str,
    name: &str,
    git_common_dir: Option<&str>,
    git_root_commits: Option<&str>,
) -> Result<i32, sqlx::Error> {
    // `last_scanned_at = NOW()` is set on both INSERT and ON CONFLICT so
    // any file processed for a project bumps its freshness signal. The
    // `update_projects_scanned_by_workspace` helper catches the
    // no-file-change case (rescan finds nothing new) by issuing a bulk
    // UPDATE keyed on `workspace_path`.
    let row = sqlx::query_scalar::<_, i32>(
        "INSERT INTO projects (workspace_path, path, name, git_common_dir, git_root_commits, last_scanned_at)
         VALUES ($1, $2, $3, $4, $5, NOW())
         ON CONFLICT (path) DO UPDATE SET
            workspace_path = EXCLUDED.workspace_path,
            name = EXCLUDED.name,
            git_common_dir = EXCLUDED.git_common_dir,
            git_root_commits = EXCLUDED.git_root_commits,
            last_scanned_at = NOW()
         RETURNING id",
    )
    .bind(workspace_path)
    .bind(path)
    .bind(name)
    .bind(git_common_dir)
    .bind(git_root_commits)
    .fetch_one(pool)
    .await?;

    Ok(row)
}

/// List all projects with file counts.
pub async fn list_projects(pool: &PgPool) -> Result<Vec<ProjectInfo>, sqlx::Error> {
    let rows = sqlx::query_as::<_, ProjectInfo>(
        "SELECT p.id, p.workspace_path, p.path, p.name,
                p.git_common_dir, p.git_root_commits,
                p.discovered_at, p.last_scanned_at,
                COUNT(f.id) as file_count
         FROM projects p
         LEFT JOIN indexed_files f ON f.project_id = p.id
         GROUP BY p.id
         ORDER BY p.name",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct ProjectInfo {
    pub id: i32,
    pub workspace_path: String,
    pub path: String,
    pub name: String,
    /// Canonical absolute path of the shared `.git` directory. Two
    /// projects with the same value are worktrees of the same repo.
    /// `None` for non-git projects.
    pub git_common_dir: Option<String>,
    /// Sorted comma-joined list of root-commit SHAs. Two projects with
    /// the same value are sibling clones (or worktrees) of the same
    /// upstream repo. `None` when the project isn't a git repo or has
    /// no commits.
    pub git_root_commits: Option<String>,
    pub discovered_at: Option<DateTime<Utc>>,
    pub last_scanned_at: Option<DateTime<Utc>>,
    pub file_count: Option<i64>,
}

/// Find the project whose path is the longest prefix of a given directory.
/// Used by the `context` CLI subcommand to identify which project the user is in.
pub async fn find_project_by_cwd(
    pool: &PgPool,
    cwd: &str,
) -> Result<Option<ProjectInfo>, sqlx::Error> {
    sqlx::query_as::<_, ProjectInfo>(
        "SELECT p.id, p.workspace_path, p.path, p.name,
                p.git_common_dir, p.git_root_commits,
                p.discovered_at, p.last_scanned_at,
                (SELECT COUNT(*) FROM indexed_files f WHERE f.project_id = p.id) AS file_count
         FROM projects p
         WHERE $1 LIKE p.path || '%'
         ORDER BY LENGTH(p.path) DESC
         LIMIT 1",
    )
    .bind(cwd)
    .fetch_optional(pool)
    .await
}

/// Resolve which projects are the **main** of their worktree group.
///
/// pgmcp indexes ~50 projects, many of which are git worktrees of the same
/// upstream (e.g. `f1r3node/`, `f1r3node-cost-accounted-rho-calc/`,
/// `f1r3node-reified-rspaces/` all share `git_common_dir`). For cross-project
/// analyses (similarity, refactoring, dependency-health) we want one canonical
/// row per worktree group; otherwise feature-branch worktrees double-count.
///
/// Grouping rule: two projects are siblings if they share `git_common_dir`
/// (canonical) or — when that's NULL — `git_root_commits` (works for non-git
/// or detached worktrees). Projects with both NULL are singletons.
///
/// Within each group of size > 1, the **main** project is the one whose
/// directory basename is shortest (with lexicographic-shortest as a
/// tiebreaker). The user's convention is that feature-branch worktrees use
/// the form `<canonical>-<feature>`, so the canonical name is always a
/// strict prefix of every sibling — and therefore the shortest basename.
///
/// Returns the list of `project_id`s that are main, sorted ascending for
/// stable downstream pagination.
pub async fn select_main_worktree_projects(pool: &PgPool) -> Result<Vec<i32>, sqlx::Error> {
    #[derive(sqlx::FromRow)]
    struct Row {
        id: i32,
        path: String,
        git_common_dir: Option<String>,
        git_root_commits: Option<String>,
    }

    let rows: Vec<Row> =
        sqlx::query_as::<_, Row>("SELECT id, path, git_common_dir, git_root_commits FROM projects")
            .fetch_all(pool)
            .await?;

    let tuples: Vec<(i32, String, Option<String>, Option<String>)> = rows
        .into_iter()
        .map(|r| (r.id, r.path, r.git_common_dir, r.git_root_commits))
        .collect();

    Ok(pick_main_worktree_ids(&tuples))
}

/// Pure grouping logic — given `(id, path, git_common_dir, git_root_commits)`
/// rows, return the main project id of each worktree group. Extracted from
/// `select_main_worktree_projects` so the algorithm can be unit-tested without
/// a Postgres instance.
pub(crate) fn pick_main_worktree_ids(
    rows: &[(i32, String, Option<String>, Option<String>)],
) -> Vec<i32> {
    use std::collections::HashMap;

    let mut groups: HashMap<(String, String), Vec<(i32, String)>> = HashMap::new();
    let mut singletons: Vec<i32> = Vec::new();

    for (id, path, git_common_dir, git_root_commits) in rows {
        let basename = path
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(path)
            .to_string();
        match (git_common_dir.as_deref(), git_root_commits.as_deref()) {
            (None, None) => singletons.push(*id),
            (cd, rc) => {
                let key = (cd.unwrap_or("").to_string(), rc.unwrap_or("").to_string());
                groups.entry(key).or_default().push((*id, basename));
            }
        }
    }

    let mut main_ids = singletons;
    for (_, mut members) in groups {
        // Shortest basename wins; tie-break lexicographically; final tie-break by id.
        members.sort_by(|a, b| {
            a.1.len()
                .cmp(&b.1.len())
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| a.0.cmp(&b.0))
        });
        if let Some((id, _)) = members.into_iter().next() {
            main_ids.push(id);
        }
    }

    main_ids.sort();
    main_ids
}

#[cfg(test)]
mod worktree_tests {
    use super::pick_main_worktree_ids;

    /// Helper to build the (id, path, common_dir, root_commits) tuple used by
    /// the resolver.
    fn row(
        id: i32,
        path: &str,
        common_dir: Option<&str>,
        root_commits: Option<&str>,
    ) -> (i32, String, Option<String>, Option<String>) {
        (
            id,
            path.to_string(),
            common_dir.map(str::to_string),
            root_commits.map(str::to_string),
        )
    }

    #[test]
    fn singleton_project_is_always_main() {
        let rows = vec![row(1, "/ws/foo", None, None)];
        assert_eq!(pick_main_worktree_ids(&rows), vec![1]);
    }

    #[test]
    fn worktree_group_sharing_git_common_dir_picks_shortest_basename() {
        // `f1r3node` < `f1r3node-cost-accounted-rho-calc` < `f1r3node-reified-rspaces`
        let rows = vec![
            row(
                10,
                "/ws/f1r3node-reified-rspaces",
                Some("/ws/f1r3node/.git"),
                None,
            ),
            row(11, "/ws/f1r3node", Some("/ws/f1r3node/.git"), None),
            row(
                12,
                "/ws/f1r3node-cost-accounted-rho-calc",
                Some("/ws/f1r3node/.git"),
                None,
            ),
        ];
        assert_eq!(pick_main_worktree_ids(&rows), vec![11]);
    }

    #[test]
    fn worktree_group_sharing_root_commits_when_common_dir_is_null() {
        // `MeTTa-Compiler/.git` is null for the worktree clones in the
        // user's setup; they're still grouped via `git_root_commits`.
        let rows = vec![
            row(20, "/ws/MeTTa-Compiler-JIT", None, Some("abc123")),
            row(21, "/ws/MeTTa-Compiler", None, Some("abc123")),
            row(22, "/ws/MeTTa-Compiler-PR-63", None, Some("abc123")),
        ];
        assert_eq!(pick_main_worktree_ids(&rows), vec![21]);
    }

    #[test]
    fn projects_with_both_keys_null_each_become_their_own_group() {
        // Two unrelated non-git projects must NOT collapse into one group;
        // each stays a singleton.
        let rows = vec![
            row(30, "/ws/alpha", None, None),
            row(31, "/ws/beta", None, None),
        ];
        let mut got = pick_main_worktree_ids(&rows);
        got.sort();
        assert_eq!(got, vec![30, 31]);
    }

    #[test]
    fn lexicographic_tiebreak_when_basenames_equal_length() {
        // Equal-length basenames (5 chars each): pick the lexicographically smallest.
        let rows = vec![
            row(40, "/ws/delta", Some("/shared/.git"), None),
            row(41, "/ws/alpha", Some("/shared/.git"), None),
        ];
        // Both "delta" and "alpha" are 5 chars; "alpha" < "delta" lexicographically.
        assert_eq!(pick_main_worktree_ids(&rows), vec![41]);
    }

    #[test]
    fn id_tiebreak_when_basename_identical() {
        // Two rows with identical basenames (path collision is degenerate
        // but possible from a stale rescan) — fall through to id ascending.
        let rows = vec![
            row(50, "/ws-a/dup", Some("/shared/.git"), None),
            row(49, "/ws-b/dup", Some("/shared/.git"), None),
        ];
        assert_eq!(pick_main_worktree_ids(&rows), vec![49]);
    }

    #[test]
    fn singletons_and_groups_combine_in_one_call() {
        let rows = vec![
            // Worktree group A: main is 100 ("rust")
            row(100, "/ws/rust", Some("/a/.git"), None),
            row(101, "/ws/rust-feature-x", Some("/a/.git"), None),
            // Singleton project
            row(200, "/ws/lonely", None, None),
            // Worktree group B keyed on root_commits: main is 301 ("py")
            row(300, "/ws/py-experimental", None, Some("xyz")),
            row(301, "/ws/py", None, Some("xyz")),
        ];
        assert_eq!(pick_main_worktree_ids(&rows), vec![100, 200, 301]);
    }

    #[test]
    fn trailing_slash_in_path_does_not_break_basename() {
        let rows = vec![row(60, "/ws/foo/", Some("/a/.git"), None)];
        // Basename of "/ws/foo/" is "foo".
        assert_eq!(pick_main_worktree_ids(&rows), vec![60]);
    }

    #[test]
    fn empty_input_returns_empty() {
        let rows: Vec<(i32, String, Option<String>, Option<String>)> = Vec::new();
        assert!(pick_main_worktree_ids(&rows).is_empty());
    }
}

/// Returns language breakdown (language, count) for a project, ordered by count descending.
pub async fn language_summary(
    pool: &PgPool,
    project_name: &str,
) -> Result<Vec<LanguageCount>, sqlx::Error> {
    sqlx::query_as::<_, LanguageCount>(
        "SELECT f.language, COUNT(*) as count
         FROM indexed_files f
         JOIN projects p ON f.project_id = p.id
         WHERE p.name = $1
         GROUP BY f.language
         ORDER BY count DESC",
    )
    .bind(project_name)
    .fetch_all(pool)
    .await
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct LanguageCount {
    pub language: String,
    pub count: i64,
}

/// Update last_scanned_at for a project.
pub async fn update_project_scanned(pool: &PgPool, project_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE projects SET last_scanned_at = NOW() WHERE id = $1")
        .bind(project_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Bulk-update `last_scanned_at = NOW()` for every project whose
/// `workspace_path` matches the given string. Used by the scanner +
/// rescan paths to mark "this workspace was fully walked at NOW()",
/// even when no files changed (so no `upsert_project` was called and
/// the per-file `last_scanned_at` update never fired).
///
/// Returns the number of rows updated.
pub async fn update_projects_scanned_by_workspace(
    pool: &PgPool,
    workspace_path: &str,
) -> Result<u64, sqlx::Error> {
    let result =
        sqlx::query("UPDATE projects SET last_scanned_at = NOW() WHERE workspace_path = $1")
            .bind(workspace_path)
            .execute(pool)
            .await?;
    Ok(result.rows_affected())
}

// ============================================================================
// Scan-time metadata (Level 1 skip check)
// ============================================================================

/// Metadata for scan-time skip decisions.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct IndexedFileMeta {
    pub path: String,
    pub modified_at: DateTime<Utc>,
    pub size_bytes: i64,
}

/// Load all indexed file metadata in a single batch query.
/// Only returns files with non-NULL content_hash (fully indexed).
pub async fn get_all_file_metadata(pool: &PgPool) -> Result<Vec<IndexedFileMeta>, sqlx::Error> {
    sqlx::query_as::<_, IndexedFileMeta>(
        "SELECT path, modified_at, size_bytes FROM indexed_files WHERE content_hash IS NOT NULL",
    )
    .fetch_all(pool)
    .await
}

// ============================================================================
// File queries
// ============================================================================

/// Upsert an indexed file.
///
/// Pass `content_hash: None` during initial insert (deferred commit);
/// the real hash is set via `finalize_file_hash` after all chunks are
/// inserted.
///
/// `content_recoverable_from_disk = true` lets the indexer skip storing
/// `content` for plain-text languages whose source file is readable from
/// disk (after `content_hash` verification). Document languages keep
/// their already-extracted text in `content` because re-running pandoc /
/// pdftotext is expensive.
pub async fn upsert_file(
    pool: &PgPool,
    project_id: i32,
    path: &str,
    relative_path: &str,
    language: &str,
    size_bytes: i64,
    content: Option<&str>,
    content_hash: Option<i64>,
    line_count: i32,
    truncated: bool,
    content_recoverable_from_disk: bool,
    modified_at: DateTime<Utc>,
) -> Result<i64, sqlx::Error> {
    let row = sqlx::query_scalar::<_, i64>(
        "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, content_hash, line_count, truncated, content_recoverable_from_disk, modified_at)
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
         RETURNING id"
    )
    .bind(project_id)
    .bind(path)
    .bind(relative_path)
    .bind(language)
    .bind(size_bytes)
    .bind(content)
    .bind(content_hash)
    .bind(line_count)
    .bind(truncated)
    .bind(content_recoverable_from_disk)
    .bind(modified_at)
    .fetch_one(pool)
    .await?;

    Ok(row)
}

/// Get the content hash for a file path (for skip-if-unchanged check).
/// Returns `None` if the file is not indexed or has a NULL hash (incomplete indexing).
pub async fn get_content_hash(pool: &PgPool, path: &str) -> Result<Option<i64>, sqlx::Error> {
    let row: Option<Option<i64>> = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT content_hash FROM indexed_files WHERE path = $1",
    )
    .bind(path)
    .fetch_optional(pool)
    .await?;

    // flatten: no row → None, row with NULL hash → None, row with hash → Some(hash)
    Ok(row.flatten())
}

/// Finalize a file's content hash after all chunks have been inserted.
/// This completes the two-phase commit: the file is now fully indexed.
pub async fn finalize_file_hash(
    pool: &PgPool,
    file_id: i64,
    content_hash: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE indexed_files SET content_hash = $1 WHERE id = $2")
        .bind(content_hash)
        .bind(file_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Delete old chunks for a file.
pub async fn delete_file_chunks(pool: &PgPool, file_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM file_chunks WHERE file_id = $1")
        .bind(file_id)
        .execute(pool)
        .await?;
    Ok(())
}

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
    // Phase 5 C3: dispatch on embedding dim. 384 → legacy `embedding`
    // column with MiniLM signature; 1024 → `embedding_v2` with
    // BGE-M3 signature. Any other dim is a configuration error
    // (model and DB out of sync) — refuse rather than silently
    // misroute. Plan reference:
    // ~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md
    // Phase 5 C3.
    let embedding_vec = pgvector::Vector::from(embedding.to_vec());
    match embedding.len() {
        384 => {
            sqlx::query(
                "INSERT INTO file_chunks
                    (file_id, chunk_index, content, start_line, end_line,
                     embedding, embedding_signature)
                 VALUES ($1, $2, $3, $4, $5, $6, 'minilm-l6-v2')
                 ON CONFLICT (file_id, chunk_index) DO UPDATE SET
                    content = EXCLUDED.content,
                    start_line = EXCLUDED.start_line,
                    end_line = EXCLUDED.end_line,
                    embedding = EXCLUDED.embedding,
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
        }
        1024 => {
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
        }
        other => {
            return Err(sqlx::Error::Protocol(format!(
                "insert_chunk: unsupported embedding dim {other} (expected 384 \
                 for MiniLM-L6-v2 or 1024 for BGE-M3); daemon model and database \
                 schema are out of sync — run `pgmcp embed-cutover --check`"
            )));
        }
    }
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
        // Phase 5 C3: dispatch on dim. Same shape as insert_chunk.
        let embedding_vec = pgvector::Vector::from(chunk.embedding.to_vec());
        let sql = match chunk.embedding.len() {
            384 => {
                "INSERT INTO file_chunks
                    (file_id, chunk_index, content, start_line, end_line,
                     embedding, embedding_signature)
                 VALUES ($1, $2, $3, $4, $5, $6, 'minilm-l6-v2')
                 ON CONFLICT (file_id, chunk_index) DO UPDATE SET
                    content = EXCLUDED.content,
                    start_line = EXCLUDED.start_line,
                    end_line = EXCLUDED.end_line,
                    embedding = EXCLUDED.embedding,
                    embedding_signature = EXCLUDED.embedding_signature"
            }
            1024 => {
                "INSERT INTO file_chunks
                    (file_id, chunk_index, content, start_line, end_line,
                     embedding_v2, embedding_signature)
                 VALUES ($1, $2, $3, $4, $5, $6, 'bge-m3-v1')
                 ON CONFLICT (file_id, chunk_index) DO UPDATE SET
                    content = EXCLUDED.content,
                    start_line = EXCLUDED.start_line,
                    end_line = EXCLUDED.end_line,
                    embedding_v2 = EXCLUDED.embedding_v2,
                    embedding_signature = EXCLUDED.embedding_signature"
            }
            other => {
                drop(tx);
                return Ok(ChunkBatchOutcome {
                    fk_violation: false,
                    error: Some(sqlx::Error::Protocol(format!(
                        "insert_chunks_batch: unsupported embedding dim {other} \
                         (expected 384 for MiniLM-L6-v2 or 1024 for BGE-M3); \
                         run `pgmcp embed-cutover --check`"
                    ))),
                });
            }
        };
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

/// Delete an indexed file and its chunks.
pub async fn delete_file(pool: &PgPool, path: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM indexed_files WHERE path = $1")
        .bind(path)
        .execute(pool)
        .await?;
    Ok(())
}

/// Batch-delete indexed files by path. Returns the number of rows deleted.
/// `ON DELETE CASCADE` on `file_chunks.file_id` handles chunk cleanup automatically.
pub async fn delete_files_batch(pool: &PgPool, paths: &[String]) -> Result<u64, sqlx::Error> {
    if paths.is_empty() {
        return Ok(0);
    }
    let result = sqlx::query("DELETE FROM indexed_files WHERE path = ANY($1)")
        .bind(paths)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

// ============================================================================
// Search queries
// ============================================================================

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct SearchResult {
    pub path: String,
    pub relative_path: String,
    pub language: String,
    pub chunk_content: String,
    pub start_line: i32,
    pub end_line: i32,
    pub score: Option<f64>,
    pub project_name: String,
}

/// SQL fragment that filters cross-worktree duplicates of the same
/// relative path. When `$N::bool` is false (the default), `NOT $N` is
/// true and the OR short-circuits — no behavioural change. When true,
/// the NOT EXISTS sub-query removes any row whose project has a
/// lower-id sibling worktree (same `git_common_dir` or
/// `git_root_commits`) that also holds a file with the same
/// relative_path. Used by all four search tools (semantic, text, grep,
/// hybrid via text+semantic).
///
/// Aliases used by the embedding query: `f` for `indexed_files`,
/// `$N` for the bound `dedupe_worktrees::bool`. Caller must supply the
/// actual `$N` index in the SQL string substitution.
const fn worktree_dedup_clause(idx: u8) -> &'static str {
    // Keep the aliases minimal — `p_dup`/`f_dup`/`p_self` are local to
    // the sub-query and don't conflict with anything in the surrounding
    // SELECT (which uses `p`, `f`, `c`).
    match idx {
        3 => DEDUP_3,
        4 => DEDUP_4,
        5 => DEDUP_5,
        _ => panic!("worktree_dedup_clause: only $3..$5 supported"),
    }
}

const DEDUP_3: &str = "(NOT $3 OR NOT EXISTS (
    SELECT 1 FROM projects p_dup
    JOIN indexed_files f_dup ON f_dup.project_id = p_dup.id
    JOIN projects p_self ON p_self.id = f.project_id
    WHERE p_dup.id < p_self.id
      AND f_dup.relative_path = f.relative_path
      AND (
          (p_dup.git_common_dir IS NOT NULL AND p_self.git_common_dir IS NOT NULL
           AND p_dup.git_common_dir = p_self.git_common_dir)
          OR
          (p_dup.git_root_commits IS NOT NULL AND p_self.git_root_commits IS NOT NULL
           AND p_dup.git_root_commits = p_self.git_root_commits)
      )
))";
const DEDUP_4: &str = "(NOT $4 OR NOT EXISTS (
    SELECT 1 FROM projects p_dup
    JOIN indexed_files f_dup ON f_dup.project_id = p_dup.id
    JOIN projects p_self ON p_self.id = f.project_id
    WHERE p_dup.id < p_self.id
      AND f_dup.relative_path = f.relative_path
      AND (
          (p_dup.git_common_dir IS NOT NULL AND p_self.git_common_dir IS NOT NULL
           AND p_dup.git_common_dir = p_self.git_common_dir)
          OR
          (p_dup.git_root_commits IS NOT NULL AND p_self.git_root_commits IS NOT NULL
           AND p_dup.git_root_commits = p_self.git_root_commits)
      )
))";
const DEDUP_5: &str = "(NOT $5 OR NOT EXISTS (
    SELECT 1 FROM projects p_dup
    JOIN indexed_files f_dup ON f_dup.project_id = p_dup.id
    JOIN projects p_self ON p_self.id = f.project_id
    WHERE p_dup.id < p_self.id
      AND f_dup.relative_path = f.relative_path
      AND (
          (p_dup.git_common_dir IS NOT NULL AND p_self.git_common_dir IS NOT NULL
           AND p_dup.git_common_dir = p_self.git_common_dir)
          OR
          (p_dup.git_root_commits IS NOT NULL AND p_self.git_root_commits IS NOT NULL
           AND p_dup.git_root_commits = p_self.git_root_commits)
      )
))";

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

    // Phase 5 C8: dispatch the query against the column whose dim
    // matches the incoming embedding. 384 → legacy `embedding`,
    // 1024 → `embedding_v2`. Mismatched dims surface a clear
    // protocol error pointing at `pgmcp embed-cutover --check`.
    let col = match embedding.len() {
        384 => "embedding",
        1024 => "embedding_v2",
        other => {
            return Err(sqlx::Error::Protocol(format!(
                "semantic_search: unsupported query-embedding dim {other} \
                 (expected 384 for MiniLM or 1024 for BGE-M3). \
                 Run `pgmcp embed-cutover --check` to inspect."
            )));
        }
    };

    // Acquire a dedicated connection so ef_search applies to our query.
    // Using SET LOCAL within a transaction keeps it scoped to this operation.
    let mut tx = pool.begin().await?;

    sqlx::query(&format!("SET LOCAL hnsw.ef_search = {}", ef_search))
        .execute(&mut *tx)
        .await?;

    // Build the query dynamically based on which filters are present.
    // The dedup clause's `$N` index is determined by how many other
    // params come before it in the bind order.
    let results = match (language, project) {
        (Some(lang), Some(proj)) => {
            // $1=embedding, $2=limit, $3=lang, $4=proj, $5=dedupe
            sqlx::query_as::<_, SearchResult>(&format!(
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
            ))
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
            sqlx::query_as::<_, SearchResult>(&format!(
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
            ))
            .bind(&embedding_vec)
            .bind(limit)
            .bind(lang)
            .bind(dedupe_worktrees)
            .fetch_all(&mut *tx)
            .await?
        }
        (None, Some(proj)) => {
            // $1=embedding, $2=limit, $3=proj, $4=dedupe
            sqlx::query_as::<_, SearchResult>(&format!(
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
            ))
            .bind(&embedding_vec)
            .bind(limit)
            .bind(proj)
            .bind(dedupe_worktrees)
            .fetch_all(&mut *tx)
            .await?
        }
        (None, None) => {
            // $1=embedding, $2=limit, $3=dedupe
            sqlx::query_as::<_, SearchResult>(&format!(
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
            ))
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
    let col = match embedding.len() {
        384 => "embedding",
        1024 => "embedding_v2",
        other => {
            return Err(sqlx::Error::Protocol(format!(
                "hybrid_search_chunks: unsupported query-embedding dim {other} \
                 (expected 384 for MiniLM or 1024 for BGE-M3)."
            )));
        }
    };
    let embedding_vec = pgvector::Vector::from(embedding.to_vec());

    let mut tx = pool.begin().await?;
    sqlx::query(&format!("SET LOCAL hnsw.ef_search = {}", ef_search))
        .execute(&mut *tx)
        .await?;

    // Optional filters collapse into one query via `($n IS NULL OR …)`.
    // RRF constant 60.0 mirrors `tool_hybrid_search::RRF_K`. The dense + lexical
    // CTEs are shared; the BGE-M3 sparse leg (Phase 2.3) is added as a third RRF
    // leg only when a query sparse vector is supplied AND the chunk has
    // `sparse_v2` (NULL-tolerant — un-backfilled chunks just miss this leg).
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
                       ts_rank(to_tsvector('english', c.content), plainto_tsquery('english', $2)) AS rank
                FROM file_chunks c
                JOIN indexed_files f ON f.id = c.file_id
                JOIN projects p ON p.id = f.project_id
                WHERE to_tsvector('english', c.content) @@ plainto_tsquery('english', $2)
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
               p.name AS project_name
        FROM fused
        JOIN file_chunks c ON c.id = fused.chunk_id
        JOIN indexed_files f ON f.id = c.file_id
        JOIN projects p ON p.id = f.project_id
        ORDER BY fused.rrf DESC
        LIMIT $6";

    let results = if let Some(sparse) = query_sparse {
        let sql = format!(
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
                       COALESCE(1.0 / (60.0 + d.rnk), 0.0)
                     + COALESCE(1.0 / (60.0 + l.rnk), 0.0)
                     + COALESCE(1.0 / (60.0 + s.rnk), 0.0) AS rrf
                FROM dense d
                FULL OUTER JOIN lexical l ON d.chunk_id = l.chunk_id
                FULL OUTER JOIN sparse s ON COALESCE(d.chunk_id, l.chunk_id) = s.chunk_id
            )
            {select_tail}"
        );
        sqlx::query_as::<_, SearchResult>(&sql)
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
        let sql = format!(
            "WITH {dense_lexical},
            fused AS (
                SELECT COALESCE(d.chunk_id, l.chunk_id) AS chunk_id,
                       COALESCE(1.0 / (60.0 + d.rnk), 0.0)
                     + COALESCE(1.0 / (60.0 + l.rnk), 0.0) AS rrf
                FROM dense d
                FULL OUTER JOIN lexical l ON d.chunk_id = l.chunk_id
            )
            {select_tail}"
        );
        sqlx::query_as::<_, SearchResult>(&sql)
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
pub async fn text_search(
    pool: &PgPool,
    query: &str,
    limit: i32,
    language: Option<&str>,
    dedupe_worktrees: bool,
) -> Result<Vec<TextSearchResult>, sqlx::Error> {
    // Strategy: rank every chunk that matches, then DISTINCT ON file_id
    // keeping the top-ranked chunk per file. `ORDER BY file_id, rank
    // DESC` lets DISTINCT ON pick the best chunk per file; the outer
    // SELECT re-sorts by rank globally and applies the limit. Chunks
    // hang off `COALESCE(duplicate_of_file_id, id)` so duplicates point
    // at canonical chunks.
    let results = if let Some(lang) = language {
        // $1=query, $2=limit, $3=lang, $4=dedupe
        sqlx::query_as::<_, TextSearchResult>(&format!(
            "SELECT path, relative_path, language, content, rank FROM (
                SELECT DISTINCT ON (f.id)
                    f.path,
                    f.relative_path,
                    f.language,
                    c.content,
                    ts_rank(to_tsvector('english', c.content), plainto_tsquery('english', $1)) AS rank
                FROM file_chunks c
                JOIN indexed_files f ON c.file_id = COALESCE(f.duplicate_of_file_id, f.id)
                WHERE to_tsvector('english', c.content) @@ plainto_tsquery('english', $1)
                  AND f.language = $3
                  AND {}
                ORDER BY f.id, rank DESC
             ) per_file
             ORDER BY rank DESC
             LIMIT $2",
            worktree_dedup_clause(4)
        ))
        .bind(query)
        .bind(limit)
        .bind(lang)
        .bind(dedupe_worktrees)
        .fetch_all(pool)
        .await?
    } else {
        // $1=query, $2=limit, $3=dedupe
        sqlx::query_as::<_, TextSearchResult>(&format!(
            "SELECT path, relative_path, language, content, rank FROM (
                SELECT DISTINCT ON (f.id)
                    f.path,
                    f.relative_path,
                    f.language,
                    c.content,
                    ts_rank(to_tsvector('english', c.content), plainto_tsquery('english', $1)) AS rank
                FROM file_chunks c
                JOIN indexed_files f ON c.file_id = COALESCE(f.duplicate_of_file_id, f.id)
                WHERE to_tsvector('english', c.content) @@ plainto_tsquery('english', $1)
                  AND {}
                ORDER BY f.id, rank DESC
             ) per_file
             ORDER BY rank DESC
             LIMIT $2",
            worktree_dedup_clause(3)
        ))
        .bind(query)
        .bind(limit)
        .bind(dedupe_worktrees)
        .fetch_all(pool)
        .await?
    };

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
pub async fn grep_search(
    pool: &PgPool,
    pattern: &str,
    glob: Option<&str>,
    limit: i32,
    dedupe_worktrees: bool,
) -> Result<Vec<GrepResult>, sqlx::Error> {
    let results = if let Some(glob_pattern) = glob {
        // Convert glob to SQL LIKE pattern.
        // $1=pattern, $2=limit, $3=like, $4=dedupe
        let like_pattern = glob_pattern.replace('*', "%").replace('?', "_");
        sqlx::query_as::<_, GrepResult>(&format!(
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
        ))
        .bind(pattern)
        .bind(limit)
        .bind(&like_pattern)
        .bind(dedupe_worktrees)
        .fetch_all(pool)
        .await?
    } else {
        // $1=pattern, $2=limit, $3=dedupe
        sqlx::query_as::<_, GrepResult>(&format!(
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
        ))
        .bind(pattern)
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

/// Read a single file's content by path. Includes the asymmetric-
/// storage flags so callers can decide whether to attempt a disk
/// fast-path before falling back to chunk stitching.
pub async fn read_file(pool: &PgPool, path: &str) -> Result<Option<FileContent>, sqlx::Error> {
    let row = sqlx::query_as::<_, FileContent>(
        "SELECT path, relative_path, language, content, size_bytes, line_count, truncated,
                content_recoverable_from_disk, content_hash
         FROM indexed_files WHERE path = $1",
    )
    .bind(path)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct FileContent {
    pub path: String,
    pub relative_path: String,
    pub language: String,
    pub content: Option<String>,
    pub size_bytes: i64,
    pub line_count: i32,
    pub truncated: bool,
    /// True when the indexer deliberately stored `content = NULL`
    /// because the source file lives on disk and is recreate-cheap.
    /// `read_file` consumers attempt `fs::read_to_string` (after a
    /// `content_hash` check) before falling back to chunk-stitching.
    #[serde(skip_serializing)]
    pub content_recoverable_from_disk: bool,
    /// xxHash3-64 of the file bytes at indexing time. Used to verify
    /// that the on-disk file matches what was indexed before serving
    /// a disk-read fast-path. `None` for in-flight / never-finalized
    /// rows; the disk fast-path skips those.
    #[serde(skip_serializing)]
    pub content_hash: Option<i64>,
}

/// Read a single file's content by relative path.
pub async fn read_file_by_relative_path(
    pool: &PgPool,
    relative_path: &str,
) -> Result<Option<FileContent>, sqlx::Error> {
    let row = sqlx::query_as::<_, FileContent>(
        "SELECT path, relative_path, language, content, size_bytes, line_count, truncated,
                content_recoverable_from_disk, content_hash
         FROM indexed_files WHERE relative_path = $1",
    )
    .bind(relative_path)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

/// Get file info/metadata.
pub async fn file_info(pool: &PgPool, path: &str) -> Result<Option<FileInfo>, sqlx::Error> {
    let row = sqlx::query_as::<_, FileInfo>(
        "SELECT path, relative_path, language, size_bytes, line_count, truncated, indexed_at, modified_at
         FROM indexed_files WHERE path = $1"
    )
    .bind(path)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct FileInfo {
    pub path: String,
    pub relative_path: String,
    pub language: String,
    pub size_bytes: i64,
    pub line_count: i32,
    pub truncated: bool,
    pub indexed_at: Option<DateTime<Utc>>,
    pub modified_at: DateTime<Utc>,
}

/// Get file tree for a project.
pub async fn project_tree(
    pool: &PgPool,
    project_name: &str,
    depth: i32,
) -> Result<Vec<String>, sqlx::Error> {
    // Get all relative paths for the project and filter by depth
    let paths = sqlx::query_scalar::<_, String>(
        "SELECT f.relative_path
         FROM indexed_files f
         JOIN projects p ON p.id = f.project_id
         WHERE p.name = $1
         ORDER BY f.relative_path",
    )
    .bind(project_name)
    .fetch_all(pool)
    .await?;

    // Filter by depth
    let filtered: Vec<String> = paths
        .into_iter()
        .filter(|p| {
            let components = p.split('/').count();
            components as i32 <= depth
        })
        .collect();

    Ok(filtered)
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
/// similar historical prompts under the given embedding signature.
///
/// `signature` is the value of `session_prompts.embedding_signature` to
/// match (e.g. `"minilm-l6-v2"` pre-cutover, `"bge-m3-v1"` post-cutover).
/// Pre-Phase-1, the column doesn't exist yet; pass `None` to skip the
/// signature filter and match the legacy 384d `embedding` column directly.
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

    // Phase 1 cutover dispatch: query length (384 vs 1024) selects the
    // column we read from. 384d → legacy MiniLM `embedding`; 1024d →
    // BGE-M3 `embedding_v2`. Other dims are rejected here so an
    // accidental mid-cutover misconfiguration surfaces as a clear error
    // instead of as wrong-shape vector arithmetic at the pgvector layer.
    //
    // Phase 5 C6 invariant: under the daemon-startup truth-table refusal
    // (`src/cli/daemon.rs::1b`), the configured `[embeddings].model`
    // and the persisted `active_embedding_signature` are guaranteed to
    // agree on dim (modulo the explicit mid-migration combination,
    // where the daemon is configured for BGE-M3 and the cron drains
    // backlog with the same model). Therefore `embedding.len()` (the
    // dim the daemon's `Embedder` produced) is provably equal to
    // `embed::signature::ActiveSignatureCache::current(pool).await?.dim()`
    // for every well-aligned daemon. The dispatch-on-len here and the
    // cache-based dispatch in C7/C8 inline-SQL tools are two sides of
    // the same invariant; the C2 cache is the single source of truth
    // for the *write* side (C3) and for tools that don't have a
    // query-vector to dispatch on (C7's centroid-aggregating tools).
    let column = match embedding.len() {
        384 => "embedding",
        1024 => "embedding_v2",
        other => {
            return Err(sqlx::Error::Protocol(format!(
                "recall_prompts: unsupported query-embedding dim {} \
                 (expected 384 for MiniLM or 1024 for BGE-M3). Run \
                 `pgmcp embed-cutover --check` to inspect.",
                other
            )));
        }
    };

    let mut tx = pool.begin().await?;
    sqlx::query(&format!("SET LOCAL hnsw.ef_search = {}", ef_search))
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

    let rows = sqlx::query_as::<_, PromptRecallResult>(&sql)
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

// ============================================================================
// Memory-server Phase 2 + 3: knowledge-graph CRUD queries
// ============================================================================
//
// Drop-in replacement surface for `@modelcontextprotocol/server-memory` —
// entities + relations + observations stored in PostgreSQL with
// bi-temporal columns. See `docs/memory-server/05-schema.md` for the
// schema and `docs/memory-server/06-tools.md` for the tool catalog.

/// Scope tuple. Each dimension is optional; NULL means "any". Used both
/// as a search filter (find entities visible under this scope) and as an
/// attachment key (create_entities attaches to this scope row).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ScopeSpec {
    pub user_id: Option<String>,
    pub agent_id: Option<String>,
    pub session_id: Option<uuid::Uuid>,
    pub project_id: Option<i32>,
}

/// Find an existing `memory_scope` row matching the spec, or create one.
/// Returns the scope id.
///
/// Postgres 15+ supports `UNIQUE NULLS NOT DISTINCT`; on older versions
/// we fall back to an `INSERT ... WHERE NOT EXISTS` race-tolerant path.
pub async fn find_or_create_scope(pool: &PgPool, scope: &ScopeSpec) -> Result<i64, sqlx::Error> {
    if let Some(id) = sqlx::query_scalar::<_, i64>(
        "SELECT id FROM memory_scope
         WHERE user_id IS NOT DISTINCT FROM $1
           AND agent_id IS NOT DISTINCT FROM $2
           AND session_id IS NOT DISTINCT FROM $3
           AND project_id IS NOT DISTINCT FROM $4
         LIMIT 1",
    )
    .bind(scope.user_id.as_deref())
    .bind(scope.agent_id.as_deref())
    .bind(scope.session_id)
    .bind(scope.project_id)
    .fetch_optional(pool)
    .await?
    {
        return Ok(id);
    }

    let id: i64 = sqlx::query_scalar(
        "INSERT INTO memory_scope (user_id, agent_id, session_id, project_id)
         VALUES ($1, $2, $3, $4)
         RETURNING id",
    )
    .bind(scope.user_id.as_deref())
    .bind(scope.agent_id.as_deref())
    .bind(scope.session_id)
    .bind(scope.project_id)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Compute a sha256 hex digest. Mirrors `sessions::prompt_sha256` but
/// kept local to this module to avoid the API surface widening.
fn observation_sha256(content: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(content.as_bytes());
    format!("{:x}", h.finalize())
}

/// `memory_create_entities` payload row.
#[derive(Debug, Clone)]
pub struct NewEntityInput {
    pub name: String,
    pub entity_type: String,
    /// Initial observations attached at entity-creation time. May be empty.
    pub observations: Vec<String>,
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct EntityRow {
    pub id: i64,
    pub name: String,
    pub entity_type: String,
    pub canonical_name: Option<String>,
    pub importance: f32,
    pub source: String,
    pub created_at: DateTime<Utc>,
    pub valid_from: DateTime<Utc>,
    pub valid_to: Option<DateTime<Utc>>,
    pub superseded_by: Option<i64>,
}

/// Create entities (and optionally initial observations) under the given
/// scope. Returns the inserted entity ids in input order. Idempotent on
/// `(name, entity_type)` when an active row exists — re-using the prior
/// id and appending observations.
pub async fn memory_create_entities(
    pool: &PgPool,
    inputs: &[NewEntityInput],
    scope_id: i64,
    source: &str,
) -> Result<Vec<i64>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let mut out = Vec::with_capacity(inputs.len());

    for input in inputs {
        // Re-use the active row if one exists; otherwise insert.
        let existing: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM memory_entities
             WHERE name = $1 AND entity_type = $2 AND valid_to IS NULL
             LIMIT 1",
        )
        .bind(&input.name)
        .bind(&input.entity_type)
        .fetch_optional(&mut *tx)
        .await?;

        let entity_id: i64 = match existing {
            Some(id) => id,
            None => {
                sqlx::query_scalar(
                    "INSERT INTO memory_entities
                        (name, entity_type, importance, source)
                     VALUES ($1, $2, 0.5, $3::memory_source)
                     RETURNING id",
                )
                .bind(&input.name)
                .bind(&input.entity_type)
                .bind(source)
                .fetch_one(&mut *tx)
                .await?
            }
        };

        // Attach scope (idempotent).
        sqlx::query(
            "INSERT INTO memory_entity_scope (entity_id, scope_id)
             VALUES ($1, $2)
             ON CONFLICT DO NOTHING",
        )
        .bind(entity_id)
        .bind(scope_id)
        .execute(&mut *tx)
        .await?;

        // Append observations (idempotent on (entity_id, content_sha256, valid_from);
        // re-creating the same observation gets eaten by the UNIQUE).
        for obs in &input.observations {
            let sha = observation_sha256(obs);
            let _ = sqlx::query(
                "INSERT INTO memory_observations
                    (entity_id, content, content_sha256, source)
                 VALUES ($1, $2, $3, $4::memory_source)
                 ON CONFLICT DO NOTHING",
            )
            .bind(entity_id)
            .bind(obs)
            .bind(&sha)
            .bind(source)
            .execute(&mut *tx)
            .await?;
        }

        out.push(entity_id);
    }

    tx.commit().await?;
    Ok(out)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NewRelationInput {
    pub from: String,
    pub to: String,
    pub relation_type: String,
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct RelationRow {
    pub id: i64,
    pub from_entity_id: i64,
    pub to_entity_id: i64,
    pub relation_type: String,
    pub importance: f32,
    pub source: String,
    pub created_at: DateTime<Utc>,
    pub valid_from: DateTime<Utc>,
    pub valid_to: Option<DateTime<Utc>>,
}

/// Create relations between existing entities (looked up by name). Returns
/// the inserted relation ids; -1 sentinel for entries whose endpoints
/// couldn't be found.
pub async fn memory_create_relations(
    pool: &PgPool,
    inputs: &[NewRelationInput],
    source: &str,
) -> Result<Vec<i64>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let mut out = Vec::with_capacity(inputs.len());

    for input in inputs {
        // Resolve endpoints (active rows only).
        let from_id: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM memory_entities WHERE name = $1 AND valid_to IS NULL LIMIT 1",
        )
        .bind(&input.from)
        .fetch_optional(&mut *tx)
        .await?;
        let to_id: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM memory_entities WHERE name = $1 AND valid_to IS NULL LIMIT 1",
        )
        .bind(&input.to)
        .fetch_optional(&mut *tx)
        .await?;
        let (Some(from_id), Some(to_id)) = (from_id, to_id) else {
            out.push(-1);
            continue;
        };
        if from_id == to_id {
            out.push(-1);
            continue;
        }

        // Existing active relation with same triple? Reuse.
        let existing: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM memory_relations
             WHERE from_entity_id = $1 AND to_entity_id = $2 AND relation_type = $3
               AND valid_to IS NULL
             LIMIT 1",
        )
        .bind(from_id)
        .bind(to_id)
        .bind(&input.relation_type)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(id) = existing {
            out.push(id);
            continue;
        }

        let id: i64 = sqlx::query_scalar(
            "INSERT INTO memory_relations
                (from_entity_id, to_entity_id, relation_type, source)
             VALUES ($1, $2, $3, $4::memory_source)
             RETURNING id",
        )
        .bind(from_id)
        .bind(to_id)
        .bind(&input.relation_type)
        .bind(source)
        .fetch_one(&mut *tx)
        .await?;
        out.push(id);
    }

    tx.commit().await?;
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct AddObservationInput {
    pub entity_name: String,
    pub contents: Vec<String>,
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct ObservationRow {
    pub id: i64,
    pub entity_id: i64,
    pub content: String,
    pub importance: f32,
    pub source: String,
    pub created_at: DateTime<Utc>,
    pub valid_from: DateTime<Utc>,
    pub valid_to: Option<DateTime<Utc>>,
}

/// Append observations to an existing entity. Returns ids of newly-inserted
/// observations (skips duplicates via the UNIQUE constraint). The caller
/// can detect missing entities by counting fewer returned ids than inputs.
pub async fn memory_add_observations(
    pool: &PgPool,
    inputs: &[AddObservationInput],
    source: &str,
) -> Result<Vec<i64>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let mut out = Vec::new();

    for input in inputs {
        let entity_id: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM memory_entities WHERE name = $1 AND valid_to IS NULL LIMIT 1",
        )
        .bind(&input.entity_name)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(entity_id) = entity_id else {
            continue;
        };

        for content in &input.contents {
            let sha = observation_sha256(content);
            let inserted: Option<i64> = sqlx::query_scalar(
                "INSERT INTO memory_observations
                    (entity_id, content, content_sha256, source)
                 VALUES ($1, $2, $3, $4::memory_source)
                 ON CONFLICT DO NOTHING
                 RETURNING id",
            )
            .bind(entity_id)
            .bind(content)
            .bind(&sha)
            .bind(source)
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(id) = inserted {
                out.push(id);
            }
        }
    }

    tx.commit().await?;
    Ok(out)
}

/// Soft-delete entities by name. Sets `valid_to = NOW()` on the active
/// row for each name; observations and relations remain queryable via
/// `memory_facts_at(t < deletion_time)` per the bi-temporal contract.
///
/// Returns the number of entity rows affected.
pub async fn memory_delete_entities(pool: &PgPool, names: &[String]) -> Result<u64, sqlx::Error> {
    if names.is_empty() {
        return Ok(0);
    }
    let res = sqlx::query(
        "UPDATE memory_entities
            SET valid_to = NOW()
          WHERE name = ANY($1) AND valid_to IS NULL",
    )
    .bind(names)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

#[derive(Debug, Clone)]
pub struct DeleteObservationInput {
    pub entity_name: String,
    pub observations: Vec<String>,
}

/// Soft-delete observations by content text under a named entity.
pub async fn memory_delete_observations(
    pool: &PgPool,
    inputs: &[DeleteObservationInput],
) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let mut affected = 0_u64;
    for input in inputs {
        let entity_id: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM memory_entities WHERE name = $1 AND valid_to IS NULL LIMIT 1",
        )
        .bind(&input.entity_name)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(entity_id) = entity_id else {
            continue;
        };
        for content in &input.observations {
            let res = sqlx::query(
                "UPDATE memory_observations
                    SET valid_to = NOW()
                  WHERE entity_id = $1 AND content = $2 AND valid_to IS NULL",
            )
            .bind(entity_id)
            .bind(content)
            .execute(&mut *tx)
            .await?;
            affected += res.rows_affected();
        }
    }
    tx.commit().await?;
    Ok(affected)
}

/// Soft-delete relations matching `(from_name, to_name, relation_type)`.
pub async fn memory_delete_relations(
    pool: &PgPool,
    inputs: &[NewRelationInput],
) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let mut affected = 0_u64;
    for input in inputs {
        let res = sqlx::query(
            "UPDATE memory_relations r
                SET valid_to = NOW()
              FROM memory_entities a, memory_entities b
              WHERE r.from_entity_id = a.id
                AND r.to_entity_id = b.id
                AND a.name = $1 AND a.valid_to IS NULL
                AND b.name = $2 AND b.valid_to IS NULL
                AND r.relation_type = $3
                AND r.valid_to IS NULL",
        )
        .bind(&input.from)
        .bind(&input.to)
        .bind(&input.relation_type)
        .execute(&mut *tx)
        .await?;
        affected += res.rows_affected();
    }
    tx.commit().await?;
    Ok(affected)
}

/// Substring/ILIKE search across entity names, types, and observation
/// content (Phase 3 baseline; semantic search is `memory_semantic_search`
/// in §3.2). Scope-filtered when `scope_id` is `Some`.
///
/// Returns the matched entities (deduped) with their observation hit
/// count. Limited to `limit` rows.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct EntitySearchHit {
    pub id: i64,
    pub name: String,
    pub entity_type: String,
    pub canonical_name: Option<String>,
    pub importance: f32,
    pub matched_observations: i64,
}

pub async fn memory_search_nodes(
    pool: &PgPool,
    query: &str,
    scope_id: Option<i64>,
    limit: i32,
) -> Result<Vec<EntitySearchHit>, sqlx::Error> {
    let like = format!("%{}%", query);
    sqlx::query_as::<_, EntitySearchHit>(
        "SELECT e.id, e.name, e.entity_type, e.canonical_name, e.importance,
                COUNT(o.id) FILTER (WHERE o.content ILIKE $1) AS matched_observations
         FROM memory_entities e
         LEFT JOIN memory_observations o
            ON o.entity_id = e.id AND o.valid_to IS NULL
         LEFT JOIN memory_entity_scope es ON es.entity_id = e.id
         WHERE e.valid_to IS NULL
           AND ($2::bigint IS NULL OR es.scope_id = $2)
           AND (
             e.name ILIKE $1
             OR e.entity_type ILIKE $1
             OR e.canonical_name ILIKE $1
             OR o.content ILIKE $1
           )
         GROUP BY e.id
         ORDER BY matched_observations DESC, e.importance DESC, e.id
         LIMIT $3",
    )
    .bind(&like)
    .bind(scope_id)
    .bind(limit.clamp(1, 500))
    .fetch_all(pool)
    .await
}

/// Read entities + their observations + their relations by name (active
/// rows only). The official server's `open_nodes`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OpenedNode {
    pub entity: EntityRow,
    pub observations: Vec<String>,
    pub relations_out: Vec<NewRelationInput>,
    pub relations_in: Vec<NewRelationInput>,
}

pub async fn memory_open_nodes(
    pool: &PgPool,
    names: &[String],
) -> Result<Vec<OpenedNode>, sqlx::Error> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let entities = sqlx::query_as::<_, EntityRow>(
        "SELECT id, name, entity_type, canonical_name, importance,
                source::text AS source, created_at, valid_from, valid_to, superseded_by
         FROM memory_entities
         WHERE name = ANY($1) AND valid_to IS NULL",
    )
    .bind(names)
    .fetch_all(pool)
    .await?;

    let mut out = Vec::with_capacity(entities.len());
    for e in entities {
        let obs: Vec<String> = sqlx::query_scalar(
            "SELECT content FROM memory_observations
             WHERE entity_id = $1 AND valid_to IS NULL
             ORDER BY created_at",
        )
        .bind(e.id)
        .fetch_all(pool)
        .await?;

        let rel_out: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT a.name AS from_name, b.name AS to_name, r.relation_type
             FROM memory_relations r
             JOIN memory_entities a ON a.id = r.from_entity_id
             JOIN memory_entities b ON b.id = r.to_entity_id
             WHERE r.from_entity_id = $1 AND r.valid_to IS NULL",
        )
        .bind(e.id)
        .fetch_all(pool)
        .await?;
        let rel_in: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT a.name AS from_name, b.name AS to_name, r.relation_type
             FROM memory_relations r
             JOIN memory_entities a ON a.id = r.from_entity_id
             JOIN memory_entities b ON b.id = r.to_entity_id
             WHERE r.to_entity_id = $1 AND r.valid_to IS NULL",
        )
        .bind(e.id)
        .fetch_all(pool)
        .await?;
        let relations_out = rel_out
            .into_iter()
            .map(|(from, to, rt)| NewRelationInput {
                from,
                to,
                relation_type: rt,
            })
            .collect();
        let relations_in = rel_in
            .into_iter()
            .map(|(from, to, rt)| NewRelationInput {
                from,
                to,
                relation_type: rt,
            })
            .collect();
        out.push(OpenedNode {
            entity: e,
            observations: obs,
            relations_out,
            relations_in,
        });
    }
    Ok(out)
}

/// Full-graph dump (active rows only) for the given scope or workspace-
/// wide when `scope_id` is `None`. Returns entities, observations, and
/// relations as parallel arrays.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MemoryGraphDump {
    pub entities: Vec<EntityRow>,
    pub observations: Vec<ObservationRow>,
    pub relations: Vec<RelationDump>,
    pub entity_count: i64,
    pub observation_count: i64,
    pub relation_count: i64,
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct RelationDump {
    pub id: i64,
    pub from_entity_id: i64,
    pub to_entity_id: i64,
    pub from_name: String,
    pub to_name: String,
    pub relation_type: String,
}

pub async fn memory_read_graph(
    pool: &PgPool,
    scope_id: Option<i64>,
    limit_entities: i32,
) -> Result<MemoryGraphDump, sqlx::Error> {
    let limit = limit_entities.clamp(1, 2000);
    let entities = sqlx::query_as::<_, EntityRow>(
        "SELECT DISTINCT e.id, e.name, e.entity_type, e.canonical_name, e.importance,
                e.source::text AS source, e.created_at, e.valid_from,
                e.valid_to, e.superseded_by
         FROM memory_entities e
         LEFT JOIN memory_entity_scope es ON es.entity_id = e.id
         WHERE e.valid_to IS NULL
           AND ($1::bigint IS NULL OR es.scope_id = $1)
         ORDER BY e.importance DESC, e.id
         LIMIT $2",
    )
    .bind(scope_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    let ids: Vec<i64> = entities.iter().map(|e| e.id).collect();
    let observations: Vec<ObservationRow> = if ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as(
            "SELECT id, entity_id, content, importance, source::text AS source,
                    created_at, valid_from, valid_to
             FROM memory_observations
             WHERE entity_id = ANY($1) AND valid_to IS NULL
             ORDER BY entity_id, created_at",
        )
        .bind(&ids)
        .fetch_all(pool)
        .await?
    };

    let relations: Vec<RelationDump> = if ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as(
            "SELECT r.id, r.from_entity_id, r.to_entity_id,
                    a.name AS from_name, b.name AS to_name, r.relation_type
             FROM memory_relations r
             JOIN memory_entities a ON a.id = r.from_entity_id
             JOIN memory_entities b ON b.id = r.to_entity_id
             WHERE r.valid_to IS NULL
               AND (r.from_entity_id = ANY($1) OR r.to_entity_id = ANY($1))",
        )
        .bind(&ids)
        .fetch_all(pool)
        .await?
    };

    let entity_count = entities.len() as i64;
    let observation_count = observations.len() as i64;
    let relation_count = relations.len() as i64;
    Ok(MemoryGraphDump {
        entities,
        observations,
        relations,
        entity_count,
        observation_count,
        relation_count,
    })
}

// ============================================================================
// Memory-server Phase 3.2: pgmcp retrieval extensions
// ============================================================================
//
// Beyond the official-compat substring `memory_search_nodes`, these
// extensions add vector / hybrid / bi-temporal / graph-traversal /
// code-anchor surfaces. See `docs/memory-server/06-tools.md` Phase 3.2.

/// Semantic search over `memory_observations.embedding` (BGE-M3 dense).
/// Returns the top-k observations matching the query embedding, joined
/// with their parent entities and scope-filtered when `scope_id` is
/// `Some`.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct MemorySemanticHit {
    pub observation_id: i64,
    pub entity_id: i64,
    pub entity_name: String,
    pub entity_type: String,
    pub content: String,
    pub importance: f32,
    pub similarity: Option<f64>,
    pub created_at: DateTime<Utc>,
}

pub async fn memory_semantic_search(
    pool: &PgPool,
    embedding: &[f32],
    scope_id: Option<i64>,
    tier: Option<&str>,
    limit: i32,
    ef_search: i32,
) -> Result<Vec<MemorySemanticHit>, sqlx::Error> {
    if embedding.len() != 1024 {
        return Err(sqlx::Error::Protocol(format!(
            "memory_semantic_search: expected 1024d embedding, got {}",
            embedding.len()
        )));
    }
    let v = pgvector::Vector::from(embedding.to_vec());
    let mut tx = pool.begin().await?;
    sqlx::query(&format!("SET LOCAL hnsw.ef_search = {}", ef_search))
        .execute(&mut *tx)
        .await?;

    let rows = sqlx::query_as::<_, MemorySemanticHit>(
        "SELECT o.id AS observation_id,
                e.id AS entity_id,
                e.name AS entity_name,
                e.entity_type,
                o.content,
                o.importance,
                1 - (o.embedding <=> $1) AS similarity,
                o.created_at
         FROM memory_observations o
         JOIN memory_entities e ON e.id = o.entity_id AND e.valid_to IS NULL
         LEFT JOIN memory_entity_scope es ON es.entity_id = e.id
         LEFT JOIN memory_entity_tier  et ON et.entity_id = e.id
         WHERE o.embedding IS NOT NULL
           AND o.valid_to IS NULL
           AND ($2::bigint IS NULL OR es.scope_id = $2)
           AND ($3::text   IS NULL OR et.tier::text = $3)
         ORDER BY o.embedding <=> $1
         LIMIT $4",
    )
    .bind(&v)
    .bind(scope_id)
    .bind(tier)
    .bind(limit.clamp(1, 200))
    .fetch_all(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(rows)
}

/// Hybrid memory search: RRF fusion of FTS over observation content +
/// dense vector cosine. Mirrors the existing `hybrid_search` (file
/// chunks) but over `memory_observations`.
pub async fn memory_hybrid_search(
    pool: &PgPool,
    query_text: &str,
    embedding: &[f32],
    scope_id: Option<i64>,
    tier: Option<&str>,
    limit: i32,
    ef_search: i32,
) -> Result<Vec<MemorySemanticHit>, sqlx::Error> {
    if embedding.len() != 1024 {
        return Err(sqlx::Error::Protocol(format!(
            "memory_hybrid_search: expected 1024d embedding, got {}",
            embedding.len()
        )));
    }
    let v = pgvector::Vector::from(embedding.to_vec());
    let k = limit.clamp(1, 200);
    let pool_size = (k * 3).clamp(20, 300);
    let mut tx = pool.begin().await?;
    sqlx::query(&format!("SET LOCAL hnsw.ef_search = {}", ef_search))
        .execute(&mut *tx)
        .await?;

    let rows = sqlx::query_as::<_, MemorySemanticHit>(
        "WITH dense AS (
            SELECT o.id, o.entity_id, o.content, o.importance, o.created_at,
                   1 - (o.embedding <=> $1) AS sim,
                   ROW_NUMBER() OVER (ORDER BY o.embedding <=> $1) AS rnk
            FROM memory_observations o
            JOIN memory_entities e ON e.id = o.entity_id AND e.valid_to IS NULL
            LEFT JOIN memory_entity_scope es ON es.entity_id = e.id
            LEFT JOIN memory_entity_tier  et ON et.entity_id = e.id
            WHERE o.embedding IS NOT NULL AND o.valid_to IS NULL
              AND ($3::bigint IS NULL OR es.scope_id = $3)
              AND ($4::text   IS NULL OR et.tier::text = $4)
            ORDER BY o.embedding <=> $1
            LIMIT $5
         ),
         sparse AS (
            SELECT o.id, o.entity_id, o.content, o.importance, o.created_at,
                   NULL::float8 AS sim,
                   ROW_NUMBER() OVER (
                       ORDER BY ts_rank_cd(
                          to_tsvector('english', o.content),
                          plainto_tsquery('english', $2)
                       ) DESC
                   ) AS rnk
            FROM memory_observations o
            JOIN memory_entities e ON e.id = o.entity_id AND e.valid_to IS NULL
            LEFT JOIN memory_entity_scope es ON es.entity_id = e.id
            LEFT JOIN memory_entity_tier  et ON et.entity_id = e.id
            WHERE o.valid_to IS NULL
              AND ($3::bigint IS NULL OR es.scope_id = $3)
              AND ($4::text   IS NULL OR et.tier::text = $4)
              AND to_tsvector('english', o.content) @@ plainto_tsquery('english', $2)
            LIMIT $5
         ),
         fused AS (
            SELECT id, entity_id, content, importance, created_at, sim,
                   SUM(1.0 / (60.0 + rnk)) AS rrf
            FROM (
                 SELECT * FROM dense
                 UNION ALL
                 SELECT * FROM sparse
            ) u
            GROUP BY id, entity_id, content, importance, created_at, sim
         )
         SELECT f.id AS observation_id,
                e.id AS entity_id,
                e.name AS entity_name,
                e.entity_type,
                f.content,
                f.importance,
                f.sim AS similarity,
                f.created_at
         FROM fused f
         JOIN memory_entities e ON e.id = f.entity_id
         ORDER BY rrf DESC
         LIMIT $6",
    )
    .bind(&v)
    .bind(query_text)
    .bind(scope_id)
    .bind(tier)
    .bind(pool_size)
    .bind(k)
    .fetch_all(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(rows)
}

/// Bi-temporal point-in-time snapshot.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MemoryFactsAtSnapshot {
    pub as_of: DateTime<Utc>,
    pub entities: Vec<EntityRow>,
    pub observations: Vec<ObservationRow>,
    pub relations: Vec<RelationDump>,
}

pub async fn memory_facts_at(
    pool: &PgPool,
    as_of: DateTime<Utc>,
    scope_id: Option<i64>,
    tier: Option<&str>,
    limit_entities: i32,
) -> Result<MemoryFactsAtSnapshot, sqlx::Error> {
    let limit = limit_entities.clamp(1, 2000);
    let entities = sqlx::query_as::<_, EntityRow>(
        "SELECT DISTINCT e.id, e.name, e.entity_type, e.canonical_name, e.importance,
                e.source::text AS source, e.created_at, e.valid_from,
                e.valid_to, e.superseded_by
         FROM memory_entities e
         LEFT JOIN memory_entity_scope es ON es.entity_id = e.id
         LEFT JOIN memory_entity_tier  et ON et.entity_id = e.id
         WHERE e.valid_from <= $1
           AND (e.valid_to IS NULL OR e.valid_to > $1)
           AND ($2::bigint IS NULL OR es.scope_id = $2)
           AND ($3::text   IS NULL OR et.tier::text = $3)
         ORDER BY e.importance DESC, e.id
         LIMIT $4",
    )
    .bind(as_of)
    .bind(scope_id)
    .bind(tier)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    let ids: Vec<i64> = entities.iter().map(|e| e.id).collect();
    let observations: Vec<ObservationRow> = if ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as(
            "SELECT id, entity_id, content, importance, source::text AS source,
                    created_at, valid_from, valid_to
             FROM memory_observations
             WHERE entity_id = ANY($1)
               AND valid_from <= $2
               AND (valid_to IS NULL OR valid_to > $2)
             ORDER BY entity_id, created_at",
        )
        .bind(&ids)
        .bind(as_of)
        .fetch_all(pool)
        .await?
    };

    let relations: Vec<RelationDump> = if ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as(
            "SELECT r.id, r.from_entity_id, r.to_entity_id,
                    a.name AS from_name, b.name AS to_name, r.relation_type
             FROM memory_relations r
             JOIN memory_entities a ON a.id = r.from_entity_id
             JOIN memory_entities b ON b.id = r.to_entity_id
             WHERE r.valid_from <= $2
               AND (r.valid_to IS NULL OR r.valid_to > $2)
               AND (r.from_entity_id = ANY($1) OR r.to_entity_id = ANY($1))",
        )
        .bind(&ids)
        .bind(as_of)
        .fetch_all(pool)
        .await?
    };

    Ok(MemoryFactsAtSnapshot {
        as_of,
        entities,
        observations,
        relations,
    })
}

/// BFS relation-traversal from one or more seed entities.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MemoryTraversalNode {
    pub entity_id: i64,
    pub name: String,
    pub entity_type: String,
    pub depth: i32,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MemoryTraversal {
    pub seeds: Vec<i64>,
    pub nodes: Vec<MemoryTraversalNode>,
    pub edges: Vec<RelationDump>,
}

pub async fn memory_relations_traverse(
    pool: &PgPool,
    seed_ids: &[i64],
    max_depth: i32,
    relation_filter: Option<&str>,
    max_nodes: i32,
) -> Result<MemoryTraversal, sqlx::Error> {
    if seed_ids.is_empty() {
        return Ok(MemoryTraversal {
            seeds: Vec::new(),
            nodes: Vec::new(),
            edges: Vec::new(),
        });
    }
    let depth_cap = max_depth.clamp(1, 6);
    let node_cap = max_nodes.clamp(1, 1000);

    let rows = sqlx::query_as::<_, (i64, String, String, i32)>(
        "WITH RECURSIVE frontier(entity_id, name, entity_type, depth) AS (
             SELECT e.id, e.name, e.entity_type, 0::int
             FROM memory_entities e
             WHERE e.id = ANY($1) AND e.valid_to IS NULL
             UNION
             SELECT e2.id, e2.name, e2.entity_type, f.depth + 1
             FROM frontier f
             JOIN memory_relations r
                  ON  (r.from_entity_id = f.entity_id OR r.to_entity_id = f.entity_id)
                  AND r.valid_to IS NULL
                  AND ($2::text IS NULL OR r.relation_type = $2)
             JOIN memory_entities e2
                  ON e2.id = CASE WHEN r.from_entity_id = f.entity_id
                                  THEN r.to_entity_id
                                  ELSE r.from_entity_id
                              END
                  AND e2.valid_to IS NULL
             WHERE f.depth < $3
         )
         SELECT entity_id, name, entity_type, MIN(depth)::int AS depth
         FROM frontier
         GROUP BY entity_id, name, entity_type
         ORDER BY MIN(depth), entity_id
         LIMIT $4",
    )
    .bind(seed_ids)
    .bind(relation_filter)
    .bind(depth_cap)
    .bind(node_cap)
    .fetch_all(pool)
    .await?;

    let nodes: Vec<MemoryTraversalNode> = rows
        .into_iter()
        .map(|(id, name, entity_type, depth)| MemoryTraversalNode {
            entity_id: id,
            name,
            entity_type,
            depth,
        })
        .collect();
    let node_ids: Vec<i64> = nodes.iter().map(|n| n.entity_id).collect();

    let edges: Vec<RelationDump> = if node_ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as(
            "SELECT r.id, r.from_entity_id, r.to_entity_id,
                    a.name AS from_name, b.name AS to_name, r.relation_type
             FROM memory_relations r
             JOIN memory_entities a ON a.id = r.from_entity_id
             JOIN memory_entities b ON b.id = r.to_entity_id
             WHERE r.valid_to IS NULL
               AND r.from_entity_id = ANY($1)
               AND r.to_entity_id = ANY($1)
               AND ($2::text IS NULL OR r.relation_type = $2)",
        )
        .bind(&node_ids)
        .bind(relation_filter)
        .fetch_all(pool)
        .await?
    };

    Ok(MemoryTraversal {
        seeds: seed_ids.to_vec(),
        nodes,
        edges,
    })
}

// ============================================================================
// Memory-server Phase 3.2: code-anchor cross-graph queries
// ============================================================================

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct MemoryCodeAnchorRow {
    pub id: i64,
    pub entity_id: i64,
    pub file_id: Option<i64>,
    pub chunk_id: Option<i64>,
    pub topic_id: Option<i64>,
    pub anchor_type: String,
    pub created_at: DateTime<Utc>,
}

pub async fn memory_anchor_entity(
    pool: &PgPool,
    entity_id: i64,
    file_id: Option<i64>,
    chunk_id: Option<i64>,
    topic_id: Option<i64>,
    anchor_type: &str,
) -> Result<i64, sqlx::Error> {
    if file_id.is_none() && chunk_id.is_none() && topic_id.is_none() {
        return Err(sqlx::Error::Protocol(
            "memory_anchor_entity: at least one of file_id/chunk_id/topic_id is required".into(),
        ));
    }
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO memory_code_anchor
            (entity_id, file_id, chunk_id, topic_id, anchor_type)
         VALUES ($1, $2, $3, $4, $5)
         RETURNING id",
    )
    .bind(entity_id)
    .bind(file_id)
    .bind(chunk_id)
    .bind(topic_id)
    .bind(anchor_type)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

pub async fn memory_unanchor_entity(pool: &PgPool, anchor_id: i64) -> Result<bool, sqlx::Error> {
    let res = sqlx::query("DELETE FROM memory_code_anchor WHERE id = $1")
        .bind(anchor_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

pub async fn memory_find_code_for_entity(
    pool: &PgPool,
    entity_id: i64,
    anchor_type: Option<&str>,
) -> Result<Vec<MemoryCodeAnchorRow>, sqlx::Error> {
    sqlx::query_as::<_, MemoryCodeAnchorRow>(
        "SELECT id, entity_id, file_id, chunk_id, topic_id, anchor_type, created_at
         FROM memory_code_anchor
         WHERE entity_id = $1
           AND ($2::text IS NULL OR anchor_type = $2)
         ORDER BY created_at DESC",
    )
    .bind(entity_id)
    .bind(anchor_type)
    .fetch_all(pool)
    .await
}

pub async fn memory_find_entities_for_code(
    pool: &PgPool,
    file_id: Option<i64>,
    chunk_id: Option<i64>,
    topic_id: Option<i64>,
) -> Result<Vec<MemoryCodeAnchorRow>, sqlx::Error> {
    let provided = [file_id.is_some(), chunk_id.is_some(), topic_id.is_some()]
        .iter()
        .filter(|b| **b)
        .count();
    if provided != 1 {
        return Err(sqlx::Error::Protocol(
            "memory_find_entities_for_code: pass exactly one of file_id, chunk_id, topic_id".into(),
        ));
    }
    sqlx::query_as::<_, MemoryCodeAnchorRow>(
        "SELECT id, entity_id, file_id, chunk_id, topic_id, anchor_type, created_at
         FROM memory_code_anchor
         WHERE ($1::bigint IS NOT NULL AND file_id  = $1)
            OR ($2::bigint IS NOT NULL AND chunk_id = $2)
            OR ($3::bigint IS NOT NULL AND topic_id = $3)
         ORDER BY created_at DESC",
    )
    .bind(file_id)
    .bind(chunk_id)
    .bind(topic_id)
    .fetch_all(pool)
    .await
}

// ============================================================================
// Memory-server Phase 6: hierarchical + graph-enhanced retrieval queries
// ============================================================================

/// Phase 6.3 result row from `memory_unified_nodes` matview.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct UnifiedNodeHit {
    pub node_id: String,
    pub node_type: String,
    pub label: String,
    pub importance: f64,
    pub similarity: Option<f64>,
}

/// Phase 6.3: vector-similarity search over the unified-nodes matview.
/// Optionally filter to a subset of node_type strings.
pub async fn memory_unified_search(
    pool: &PgPool,
    embedding: &[f32],
    node_types: Option<&[String]>,
    limit: i32,
    ef_search: i32,
) -> Result<Vec<UnifiedNodeHit>, sqlx::Error> {
    if embedding.len() != 1024 {
        return Err(sqlx::Error::Protocol(format!(
            "memory_unified_search: expected 1024d embedding, got {}",
            embedding.len()
        )));
    }
    let v = pgvector::Vector::from(embedding.to_vec());
    let mut tx = pool.begin().await?;
    sqlx::query(&format!("SET LOCAL hnsw.ef_search = {}", ef_search))
        .execute(&mut *tx)
        .await?;
    let rows = sqlx::query_as::<_, UnifiedNodeHit>(
        "SELECT node_id, node_type, label, importance,
                1 - (embedding <=> $1) AS similarity
         FROM memory_unified_nodes
         WHERE embedding IS NOT NULL
           AND ($2::text[] IS NULL OR node_type = ANY($2))
         ORDER BY embedding <=> $1
         LIMIT $3",
    )
    .bind(&v)
    .bind(node_types)
    .bind(limit.clamp(1, 200))
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows)
}

/// Phase 6.3: refresh the materialized view. Cheap relative to topic
/// clustering — a UNION ALL over indexed tables. Called from
/// `similarity-scan` cadence or on-demand by the operator.
pub async fn refresh_memory_unified_nodes(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query("REFRESH MATERIALIZED VIEW memory_unified_nodes")
        .execute(pool)
        .await?;
    Ok(())
}

/// Phase 6.3: BFS neighbors of a typed node over `memory_unified_edges`.
/// Returns the reachable nodes up to `depth` plus the edges that connect
/// them.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct UnifiedNeighborNode {
    pub node_id: String,
    pub node_type: String,
    pub depth: i32,
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct UnifiedEdge {
    pub from_id: String,
    pub from_type: String,
    pub to_id: String,
    pub to_type: String,
    pub edge_type: String,
    pub weight: f64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct UnifiedNeighborhood {
    pub seed: String,
    pub nodes: Vec<UnifiedNeighborNode>,
    pub edges: Vec<UnifiedEdge>,
}

pub async fn memory_neighbors(
    pool: &PgPool,
    node_id: &str,
    depth: i32,
    edge_filter: Option<&str>,
    max_nodes: i32,
) -> Result<UnifiedNeighborhood, sqlx::Error> {
    let depth_cap = depth.clamp(1, 4);
    let node_cap = max_nodes.clamp(1, 500);
    let rows: Vec<(String, String, i32)> = sqlx::query_as(
        "WITH RECURSIVE frontier(node_id, node_type, depth) AS (
             SELECT node_id, node_type, 0::int
             FROM memory_unified_nodes
             WHERE node_id = $1
             UNION
             SELECT CASE WHEN e.from_id = f.node_id THEN e.to_id ELSE e.from_id END,
                    CASE WHEN e.from_id = f.node_id THEN e.to_type ELSE e.from_type END,
                    f.depth + 1
             FROM frontier f
             JOIN memory_unified_edges e
                  ON  (e.from_id = f.node_id OR e.to_id = f.node_id)
                  AND ($2::text IS NULL OR e.edge_type = $2)
             WHERE f.depth < $3
         )
         SELECT node_id, node_type, MIN(depth)::int AS depth
         FROM frontier
         GROUP BY node_id, node_type
         ORDER BY MIN(depth), node_id
         LIMIT $4",
    )
    .bind(node_id)
    .bind(edge_filter)
    .bind(depth_cap)
    .bind(node_cap)
    .fetch_all(pool)
    .await?;

    let nodes: Vec<UnifiedNeighborNode> = rows
        .into_iter()
        .map(|(id, t, d)| UnifiedNeighborNode {
            node_id: id,
            node_type: t,
            depth: d,
        })
        .collect();
    let node_ids: Vec<String> = nodes.iter().map(|n| n.node_id.clone()).collect();
    let edges: Vec<UnifiedEdge> = if node_ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as(
            "SELECT from_id, from_type, to_id, to_type, edge_type, weight
             FROM memory_unified_edges
             WHERE from_id = ANY($1) AND to_id = ANY($1)
               AND ($2::text IS NULL OR edge_type = $2)",
        )
        .bind(&node_ids)
        .bind(edge_filter)
        .fetch_all(pool)
        .await?
    };

    Ok(UnifiedNeighborhood {
        seed: node_id.to_string(),
        nodes,
        edges,
    })
}

/// Phase 6.4 PathRAG: ranked paths through the unified graph. Seeds
/// from `memory_unified_search`, then BFS-expands within
/// `max_hops`, ranks by a composite (cosine of last-node vs query,
/// minus hop-length penalty, plus edge-weight product), and prunes
/// near-duplicate paths via Jaccard overlap on the node-set.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MemoryPath {
    /// node_ids in order, starting from the seed.
    pub nodes: Vec<String>,
    pub edge_types: Vec<String>,
    pub score: f64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MemoryPathSearchResult {
    pub seeds: Vec<String>,
    pub paths: Vec<MemoryPath>,
    /// Paths considered before pruning (telemetry).
    pub considered: i64,
    pub pruned: i64,
}

pub async fn memory_path_search(
    pool: &PgPool,
    embedding: &[f32],
    seed_node_types: Option<&[String]>,
    target_node_types: Option<&[String]>,
    max_hops: i32,
    k: i32,
    prune_jaccard: f64,
    ef_search: i32,
) -> Result<MemoryPathSearchResult, sqlx::Error> {
    let hop_cap = max_hops.clamp(1, 5);
    let k = k.clamp(1, 100);

    // 1. Seed by top-k semantic over unified-nodes (k seeds = k).
    let seeds =
        memory_unified_search(pool, embedding, seed_node_types, k.max(5), ef_search).await?;
    if seeds.is_empty() {
        return Ok(MemoryPathSearchResult {
            seeds: Vec::new(),
            paths: Vec::new(),
            considered: 0,
            pruned: 0,
        });
    }
    let seed_ids: Vec<String> = seeds.iter().map(|s| s.node_id.clone()).collect();

    // 2. BFS-expand and emit complete paths. Bound output via hop_cap
    // (worst-case branching is bounded since each step joins through
    // `memory_unified_edges`, which is already capped by the membership
    // and code_anchor filters). LIMIT 400 keeps it sane.
    let rows: Vec<(String, String, String, String, String, f64, i32)> = sqlx::query_as(
        "WITH RECURSIVE walk(start_id, current_id, current_type,
                              last_edge, last_to_type, weight_product, hops,
                              path_nodes, path_edges) AS (
             SELECT s.node_id, s.node_id, s.node_type,
                    ''::text, s.node_type, 1.0::float8, 0::int,
                    ARRAY[s.node_id], ARRAY[]::text[]
             FROM memory_unified_nodes s
             WHERE s.node_id = ANY($1)
             UNION
             SELECT w.start_id,
                    CASE WHEN e.from_id = w.current_id THEN e.to_id ELSE e.from_id END,
                    CASE WHEN e.from_id = w.current_id THEN e.to_type ELSE e.from_type END,
                    e.edge_type,
                    CASE WHEN e.from_id = w.current_id THEN e.to_type ELSE e.from_type END,
                    w.weight_product * e.weight,
                    w.hops + 1,
                    w.path_nodes || (CASE WHEN e.from_id = w.current_id THEN e.to_id ELSE e.from_id END),
                    w.path_edges || e.edge_type
             FROM walk w
             JOIN memory_unified_edges e
                  ON e.from_id = w.current_id OR e.to_id = w.current_id
             WHERE w.hops < $2
               AND NOT (
                   CASE WHEN e.from_id = w.current_id THEN e.to_id ELSE e.from_id END
                       = ANY(w.path_nodes)
               )
         )
         SELECT start_id, current_id, current_type, last_edge, last_to_type,
                weight_product, hops
         FROM walk
         WHERE hops > 0
           AND ($3::text[] IS NULL OR current_type = ANY($3))
         ORDER BY hops, weight_product DESC
         LIMIT 400",
    )
    .bind(&seed_ids)
    .bind(hop_cap)
    .bind(target_node_types)
    .fetch_all(pool)
    .await?;
    let considered = rows.len() as i64;

    // We need the actual path nodes to render paths cleanly. Re-query
    // a richer set including the path_nodes / path_edges arrays.
    let path_rows: Vec<(Vec<String>, Vec<String>, f64, i32)> = sqlx::query_as(
        "WITH RECURSIVE walk(start_id, current_id, weight_product, hops,
                              path_nodes, path_edges) AS (
             SELECT s.node_id, s.node_id, 1.0::float8, 0::int,
                    ARRAY[s.node_id], ARRAY[]::text[]
             FROM memory_unified_nodes s
             WHERE s.node_id = ANY($1)
             UNION
             SELECT w.start_id,
                    CASE WHEN e.from_id = w.current_id THEN e.to_id ELSE e.from_id END,
                    w.weight_product * e.weight,
                    w.hops + 1,
                    w.path_nodes || (CASE WHEN e.from_id = w.current_id THEN e.to_id ELSE e.from_id END),
                    w.path_edges || e.edge_type
             FROM walk w
             JOIN memory_unified_edges e
                  ON e.from_id = w.current_id OR e.to_id = w.current_id
             WHERE w.hops < $2
               AND NOT (
                   CASE WHEN e.from_id = w.current_id THEN e.to_id ELSE e.from_id END
                       = ANY(w.path_nodes)
               )
         ),
         filtered AS (
             SELECT path_nodes, path_edges, weight_product, hops
             FROM walk
             JOIN memory_unified_nodes n ON n.node_id = walk.current_id
             WHERE hops > 0
               AND ($3::text[] IS NULL OR n.node_type = ANY($3))
         )
         SELECT path_nodes, path_edges, weight_product, hops
         FROM filtered
         ORDER BY weight_product DESC, hops
         LIMIT 200",
    )
    .bind(&seed_ids)
    .bind(hop_cap)
    .bind(target_node_types)
    .fetch_all(pool)
    .await?;

    // 3. Score each path. Composite: weight_product − 0.1·hops (we
    // don't have the query embedding cosine for intermediate nodes
    // cheaply; the seed cosine is baked into `seeds[i].similarity`,
    // which we incorporate by weighting the start-seed similarity).
    let seed_sim_map: std::collections::HashMap<String, f64> = seeds
        .iter()
        .map(|s| (s.node_id.clone(), s.similarity.unwrap_or(0.0)))
        .collect();

    let mut scored: Vec<MemoryPath> = Vec::with_capacity(path_rows.len());
    for (nodes, edges, weight_product, hops) in path_rows {
        let seed_sim = nodes
            .first()
            .and_then(|id| seed_sim_map.get(id))
            .copied()
            .unwrap_or(0.0);
        let score = 0.6 * seed_sim + 0.3 * weight_product - 0.1 * (hops as f64);
        scored.push(MemoryPath {
            nodes,
            edge_types: edges,
            score,
        });
    }
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // 4. PathRAG flow-style pruning: drop paths whose node-set
    // overlaps a kept path's node-set above `prune_jaccard`.
    let mut kept: Vec<MemoryPath> = Vec::with_capacity(k as usize);
    let mut pruned = 0_i64;
    for p in scored {
        let pset: std::collections::BTreeSet<&String> = p.nodes.iter().collect();
        let mut overlaps = false;
        for q in &kept {
            let qset: std::collections::BTreeSet<&String> = q.nodes.iter().collect();
            let inter = pset.intersection(&qset).count() as f64;
            let union = pset.union(&qset).count() as f64;
            let jacc = if union > 0.0 { inter / union } else { 0.0 };
            if jacc >= prune_jaccard {
                overlaps = true;
                pruned += 1;
                break;
            }
        }
        if !overlaps {
            kept.push(p);
            if kept.len() as i32 >= k {
                break;
            }
        }
    }

    Ok(MemoryPathSearchResult {
        seeds: seed_ids,
        paths: kept,
        considered,
        pruned,
    })
}

/// Phase 6.2 HippoRAG-style PPR result row.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PprHit {
    pub entity_id: i64,
    pub entity_name: String,
    pub entity_type: String,
    pub ppr_score: f64,
    pub top_observation: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PprSearchResult {
    pub seeds: Vec<i64>,
    pub hits: Vec<PprHit>,
}

/// Phase 6.2: HippoRAG-style Personalized PageRank over `memory_relations`.
/// Seeds are the top-k entities by cosine similarity of their best
/// observation against the query embedding.
pub async fn memory_ppr_search(
    pool: &PgPool,
    embedding: &[f32],
    k: i32,
    alpha: f64,
    max_seeds: i32,
    ef_search: i32,
) -> Result<PprSearchResult, sqlx::Error> {
    if embedding.len() != 1024 {
        return Err(sqlx::Error::Protocol(format!(
            "memory_ppr_search: expected 1024d embedding, got {}",
            embedding.len()
        )));
    }
    let v = pgvector::Vector::from(embedding.to_vec());
    let mut tx = pool.begin().await?;
    sqlx::query(&format!("SET LOCAL hnsw.ef_search = {}", ef_search))
        .execute(&mut *tx)
        .await?;

    // 1. Resolve seed entities by best-per-entity cosine of their
    // observations against the query embedding.
    #[derive(sqlx::FromRow)]
    struct Seed {
        entity_id: i64,
        sim: Option<f64>,
    }
    let seeds: Vec<Seed> = sqlx::query_as(
        "SELECT DISTINCT ON (e.id) e.id AS entity_id, 1 - (o.embedding <=> $1) AS sim
         FROM memory_observations o
         JOIN memory_entities e ON e.id = o.entity_id AND e.valid_to IS NULL
         WHERE o.embedding IS NOT NULL AND o.valid_to IS NULL
         ORDER BY e.id, o.embedding <=> $1
         LIMIT $2",
    )
    .bind(&v)
    .bind(max_seeds.clamp(1, 100))
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    let seed_ids: Vec<i64> = seeds.iter().map(|s| s.entity_id).collect();
    if seed_ids.is_empty() {
        return Ok(PprSearchResult {
            seeds: Vec::new(),
            hits: Vec::new(),
        });
    }

    // 2. Load the relation graph into a petgraph.
    let edges: Vec<(i64, i64, f32)> = sqlx::query_as(
        "SELECT from_entity_id, to_entity_id, importance
         FROM memory_relations
         WHERE valid_to IS NULL",
    )
    .fetch_all(pool)
    .await?;
    let mut node_to_idx: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
    let mut idx_to_node: Vec<i64> = Vec::new();
    let mut adjacency: Vec<Vec<(usize, f64)>> = Vec::new();
    let ensure_idx = |n: i64,
                      node_to_idx: &mut std::collections::HashMap<i64, usize>,
                      idx_to_node: &mut Vec<i64>,
                      adj: &mut Vec<Vec<(usize, f64)>>|
     -> usize {
        if let Some(&idx) = node_to_idx.get(&n) {
            return idx;
        }
        let idx = idx_to_node.len();
        node_to_idx.insert(n, idx);
        idx_to_node.push(n);
        adj.push(Vec::new());
        idx
    };
    for (from, to, w) in edges {
        let fi = ensure_idx(from, &mut node_to_idx, &mut idx_to_node, &mut adjacency);
        let ti = ensure_idx(to, &mut node_to_idx, &mut idx_to_node, &mut adjacency);
        let w = w as f64;
        adjacency[fi].push((ti, w));
        adjacency[ti].push((fi, w));
    }
    // Make sure every seed is present in the graph (some may have no
    // outgoing relations; they're still valid restart nodes for PPR).
    for &sid in &seed_ids {
        ensure_idx(sid, &mut node_to_idx, &mut idx_to_node, &mut adjacency);
    }

    let n = idx_to_node.len();
    if n == 0 {
        return Ok(PprSearchResult {
            seeds: seed_ids,
            hits: Vec::new(),
        });
    }

    // 3. Power iteration: PR(v) = α · (Σ PR(u)·w(u,v)/d(u)) + (1-α) · r(v),
    // where r is the restart distribution concentrated on the seeds.
    let mut restart = vec![0.0_f64; n];
    let seed_indices: Vec<usize> = seed_ids
        .iter()
        .filter_map(|&id| node_to_idx.get(&id).copied())
        .collect();
    let restart_mass = 1.0 / seed_indices.len() as f64;
    for &si in &seed_indices {
        restart[si] = restart_mass;
    }
    let mut rank = restart.clone();
    // Precompute row sums for normalization.
    let row_sums: Vec<f64> = adjacency
        .iter()
        .map(|row| row.iter().map(|(_, w)| *w).sum::<f64>().max(1e-12))
        .collect();

    let iters = 25_usize;
    for _ in 0..iters {
        let mut next = vec![0.0_f64; n];
        for (u, neighbors) in adjacency.iter().enumerate() {
            if rank[u] == 0.0 {
                continue;
            }
            let share = rank[u] / row_sums[u];
            for (v, w) in neighbors {
                next[*v] += alpha * share * *w;
            }
        }
        for i in 0..n {
            next[i] += (1.0 - alpha) * restart[i];
        }
        rank = next;
    }

    // 4. Take top-k by PR score, attach entity metadata + top observation.
    let mut ranked: Vec<(usize, f64)> = rank.iter().enumerate().map(|(i, r)| (i, *r)).collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked.truncate(k.clamp(1, 200) as usize);

    let top_ids: Vec<i64> = ranked.iter().map(|(i, _)| idx_to_node[*i]).collect();
    let mut hits: Vec<PprHit> = Vec::with_capacity(top_ids.len());
    if !top_ids.is_empty() {
        let rows: Vec<(i64, String, String, Option<String>)> = sqlx::query_as(
            "SELECT e.id, e.name, e.entity_type,
                    (SELECT o.content FROM memory_observations o
                     WHERE o.entity_id = e.id AND o.valid_to IS NULL
                     ORDER BY o.importance DESC, o.created_at DESC
                     LIMIT 1)
             FROM memory_entities e
             WHERE e.id = ANY($1) AND e.valid_to IS NULL",
        )
        .bind(&top_ids)
        .fetch_all(pool)
        .await?;
        let score_map: std::collections::HashMap<i64, f64> =
            ranked.iter().map(|(i, r)| (idx_to_node[*i], *r)).collect();
        for (id, name, etype, top_obs) in rows {
            hits.push(PprHit {
                entity_id: id,
                entity_name: name,
                entity_type: etype,
                ppr_score: *score_map.get(&id).unwrap_or(&0.0),
                top_observation: top_obs,
            });
        }
        hits.sort_by(|a, b| {
            b.ppr_score
                .partial_cmp(&a.ppr_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    Ok(PprSearchResult {
        seeds: seed_ids,
        hits,
    })
}

/// Phase 6.1 RAPTOR query result.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct RaptorHit {
    pub node_id: i64,
    pub level: i32,
    pub label: String,
    pub similarity: Option<f64>,
}

/// Phase 6.1: query against `memory_summary_tree`. Returns top-k
/// summary nodes at each requested level (or all levels), ranked
/// by cosine over `summary_embedding`. Useful for "thematic"
/// queries that span many observations.
pub async fn memory_raptor_search(
    pool: &PgPool,
    embedding: &[f32],
    scope_id: Option<i64>,
    levels: Option<&[i32]>,
    k: i32,
    ef_search: i32,
) -> Result<Vec<RaptorHit>, sqlx::Error> {
    if embedding.len() != 1024 {
        return Err(sqlx::Error::Protocol(format!(
            "memory_raptor_search: expected 1024d embedding, got {}",
            embedding.len()
        )));
    }
    let v = pgvector::Vector::from(embedding.to_vec());
    let mut tx = pool.begin().await?;
    sqlx::query(&format!("SET LOCAL hnsw.ef_search = {}", ef_search))
        .execute(&mut *tx)
        .await?;
    let rows = sqlx::query_as::<_, RaptorHit>(
        "SELECT id AS node_id, level,
                COALESCE(summary_text, '<leaf>') AS label,
                1 - (summary_embedding <=> $1) AS similarity
         FROM memory_summary_tree
         WHERE summary_embedding IS NOT NULL
           AND ($2::bigint IS NULL OR scope_id = $2)
           AND ($3::int[] IS NULL OR level = ANY($3))
         ORDER BY summary_embedding <=> $1
         LIMIT $4",
    )
    .bind(&v)
    .bind(scope_id)
    .bind(levels)
    .bind(k.clamp(1, 200))
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows)
}

// ============================================================================
// Memory-server Phase 8: forget + retention queries
// ============================================================================

/// What kind of memory row to forget. Used by `memory_forget` and the
/// audit log.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForgetTargetType {
    Entity,
    Observation,
    Relation,
}

impl ForgetTargetType {
    pub fn label(self) -> &'static str {
        match self {
            Self::Entity => "entity",
            Self::Observation => "observation",
            Self::Relation => "relation",
        }
    }
    pub fn parse(s: &str) -> Result<Self, sqlx::Error> {
        match s {
            "entity" => Ok(Self::Entity),
            "observation" => Ok(Self::Observation),
            "relation" => Ok(Self::Relation),
            other => Err(sqlx::Error::Protocol(format!(
                "unknown target_type '{}'; expected entity|observation|relation",
                other
            ))),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ForgetReport {
    pub target_type: String,
    pub target_id: i64,
    pub cascade: bool,
    pub rows_affected: i64,
    pub manifest: serde_json::Value,
    pub forget_log_id: i64,
}

/// Phase 8.4: forget an entity / observation / relation. `cascade=false`
/// (default) sets `valid_to = NOW()` (soft delete); `cascade=true`
/// physically deletes the row + dependent rows and writes the manifest
/// to `memory_forget_log`.
pub async fn memory_forget(
    pool: &PgPool,
    target_type: ForgetTargetType,
    target_id: i64,
    cascade: bool,
    actor: &str,
) -> Result<ForgetReport, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let rows_affected: i64;
    let mut manifest = serde_json::json!({});

    match target_type {
        ForgetTargetType::Entity => {
            if cascade {
                let (obs_count,): (i64,) =
                    sqlx::query_as("SELECT COUNT(*) FROM memory_observations WHERE entity_id = $1")
                        .bind(target_id)
                        .fetch_one(&mut *tx)
                        .await?;
                let (rel_count,): (i64,) = sqlx::query_as(
                    "SELECT COUNT(*) FROM memory_relations
                     WHERE from_entity_id = $1 OR to_entity_id = $1",
                )
                .bind(target_id)
                .fetch_one(&mut *tx)
                .await?;
                let (anchor_count,): (i64,) =
                    sqlx::query_as("SELECT COUNT(*) FROM memory_code_anchor WHERE entity_id = $1")
                        .bind(target_id)
                        .fetch_one(&mut *tx)
                        .await?;
                let (scope_count,): (i64,) =
                    sqlx::query_as("SELECT COUNT(*) FROM memory_entity_scope WHERE entity_id = $1")
                        .bind(target_id)
                        .fetch_one(&mut *tx)
                        .await?;
                let (tier_count,): (i64,) =
                    sqlx::query_as("SELECT COUNT(*) FROM memory_entity_tier WHERE entity_id = $1")
                        .bind(target_id)
                        .fetch_one(&mut *tx)
                        .await?;
                manifest = serde_json::json!({
                    "observations": obs_count,
                    "relations": rel_count,
                    "code_anchors": anchor_count,
                    "scopes": scope_count,
                    "tiers": tier_count,
                });
                let res = sqlx::query("DELETE FROM memory_entities WHERE id = $1")
                    .bind(target_id)
                    .execute(&mut *tx)
                    .await?;
                rows_affected = res.rows_affected() as i64
                    + obs_count
                    + rel_count
                    + anchor_count
                    + scope_count
                    + tier_count;
            } else {
                let res = sqlx::query(
                    "UPDATE memory_entities SET valid_to = NOW()
                     WHERE id = $1 AND valid_to IS NULL",
                )
                .bind(target_id)
                .execute(&mut *tx)
                .await?;
                rows_affected = res.rows_affected() as i64;
            }
        }
        ForgetTargetType::Observation => {
            if cascade {
                let res = sqlx::query("DELETE FROM memory_observations WHERE id = $1")
                    .bind(target_id)
                    .execute(&mut *tx)
                    .await?;
                rows_affected = res.rows_affected() as i64;
            } else {
                let res = sqlx::query(
                    "UPDATE memory_observations SET valid_to = NOW()
                     WHERE id = $1 AND valid_to IS NULL",
                )
                .bind(target_id)
                .execute(&mut *tx)
                .await?;
                rows_affected = res.rows_affected() as i64;
            }
        }
        ForgetTargetType::Relation => {
            if cascade {
                let res = sqlx::query("DELETE FROM memory_relations WHERE id = $1")
                    .bind(target_id)
                    .execute(&mut *tx)
                    .await?;
                rows_affected = res.rows_affected() as i64;
            } else {
                let res = sqlx::query(
                    "UPDATE memory_relations SET valid_to = NOW()
                     WHERE id = $1 AND valid_to IS NULL",
                )
                .bind(target_id)
                .execute(&mut *tx)
                .await?;
                rows_affected = res.rows_affected() as i64;
            }
        }
    };

    let forget_log_id: i64 = sqlx::query_scalar(
        "INSERT INTO memory_forget_log
            (actor, target_type, target_id, cascade, rows_affected, manifest_json)
         VALUES ($1, $2, $3, $4, $5, $6)
         RETURNING id",
    )
    .bind(actor)
    .bind(target_type.label())
    .bind(target_id)
    .bind(cascade)
    .bind(rows_affected as i32)
    .bind(&manifest)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(ForgetReport {
        target_type: target_type.label().to_string(),
        target_id,
        cascade,
        rows_affected,
        manifest,
        forget_log_id,
    })
}

/// Phase 8.2 dry-run for the retention cron. Returns counts of rows
/// that *would* be hard-deleted given the window + importance
/// threshold, without touching any rows.
pub async fn memory_retention_dry_run(
    pool: &PgPool,
    window_days: i64,
    importance_threshold: f32,
) -> Result<(i64, i64, i64), sqlx::Error> {
    let (e,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM memory_entities
         WHERE valid_to IS NOT NULL
           AND valid_to < NOW() - ($1::int * interval '1 day')
           AND importance < $2",
    )
    .bind(window_days as i32)
    .bind(importance_threshold)
    .fetch_one(pool)
    .await?;
    let (o,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM memory_observations
         WHERE valid_to IS NOT NULL
           AND valid_to < NOW() - ($1::int * interval '1 day')
           AND importance < $2",
    )
    .bind(window_days as i32)
    .bind(importance_threshold)
    .fetch_one(pool)
    .await?;
    let (r,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM memory_relations
         WHERE valid_to IS NOT NULL
           AND valid_to < NOW() - ($1::int * interval '1 day')
           AND importance < $2",
    )
    .bind(window_days as i32)
    .bind(importance_threshold)
    .fetch_one(pool)
    .await?;
    Ok((e, o, r))
}

/// Phase 8.2: hard-delete soft-deleted rows past the retention window
/// AND below the importance threshold AND not pointed at by any
/// `superseded_by` chain. Returns (entities, observations, relations)
/// deleted.
pub async fn memory_retention_purge(
    pool: &PgPool,
    window_days: i64,
    importance_threshold: f32,
) -> Result<(u64, u64, u64), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let e = sqlx::query(
        "DELETE FROM memory_entities
         WHERE valid_to IS NOT NULL
           AND valid_to < NOW() - ($1::int * interval '1 day')
           AND importance < $2
           AND id NOT IN (
               SELECT superseded_by FROM memory_entities
               WHERE superseded_by IS NOT NULL
           )",
    )
    .bind(window_days as i32)
    .bind(importance_threshold)
    .execute(&mut *tx)
    .await?;
    let o = sqlx::query(
        "DELETE FROM memory_observations
         WHERE valid_to IS NOT NULL
           AND valid_to < NOW() - ($1::int * interval '1 day')
           AND importance < $2
           AND id NOT IN (
               SELECT superseded_by FROM memory_observations
               WHERE superseded_by IS NOT NULL
           )",
    )
    .bind(window_days as i32)
    .bind(importance_threshold)
    .execute(&mut *tx)
    .await?;
    let r = sqlx::query(
        "DELETE FROM memory_relations
         WHERE valid_to IS NOT NULL
           AND valid_to < NOW() - ($1::int * interval '1 day')
           AND importance < $2
           AND id NOT IN (
               SELECT superseded_by FROM memory_relations
               WHERE superseded_by IS NOT NULL
           )",
    )
    .bind(window_days as i32)
    .bind(importance_threshold)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((e.rows_affected(), o.rows_affected(), r.rows_affected()))
}

/// Phase 9: memory-server invariant report. Each field is the count of
/// rows that violate the corresponding invariant. A clean memory graph
/// returns zeros across the board.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct MemoryEvalReport {
    /// Rows where `valid_to <= valid_from` (impossible by design).
    pub entities_temporal_invalid: i64,
    pub observations_temporal_invalid: i64,
    pub relations_temporal_invalid: i64,
    /// `superseded_by` chains that include a cycle (root reaches itself).
    pub entity_supersede_cycles: i64,
    pub observation_supersede_cycles: i64,
    pub relation_supersede_cycles: i64,
    /// Observations whose `entity_id` does not match any entity row
    /// (would normally be caught by FK; included for defense in depth).
    pub orphan_observations: i64,
    /// `derived_from` arrays in reflective observations that point at
    /// rows that no longer exist — purely an audit metric, not a fault.
    pub reflection_derived_from_missing: i64,
    /// Code-anchors whose target file/chunk/topic no longer exists.
    pub stale_code_anchors: i64,
    /// `memory_forget_log` entries whose claimed `target_id` still
    /// exists in the target table with `valid_to IS NULL` (suggests
    /// the forget didn't actually take effect).
    pub forget_log_dangling: i64,
    pub rows_examined: i64,
}

/// Phase 9: scan the memory tables for bi-temporal / provenance /
/// referential-integrity violations. Bounded by `row_cap` per table —
/// the count fields are exact within that bound, so a daemon with a
/// 50-million-row memory graph still finishes in seconds.
pub async fn memory_eval_invariants(
    pool: &PgPool,
    row_cap: i64,
) -> Result<MemoryEvalReport, sqlx::Error> {
    let mut r = MemoryEvalReport {
        rows_examined: row_cap,
        ..Default::default()
    };

    r.entities_temporal_invalid = sqlx::query_scalar(
        "SELECT COUNT(*) FROM (
           SELECT 1 FROM memory_entities
            WHERE valid_to IS NOT NULL AND valid_to <= valid_from
            LIMIT $1
         ) sub",
    )
    .bind(row_cap)
    .fetch_one(pool)
    .await?;
    r.observations_temporal_invalid = sqlx::query_scalar(
        "SELECT COUNT(*) FROM (
           SELECT 1 FROM memory_observations
            WHERE valid_to IS NOT NULL AND valid_to <= valid_from
            LIMIT $1
         ) sub",
    )
    .bind(row_cap)
    .fetch_one(pool)
    .await?;
    r.relations_temporal_invalid = sqlx::query_scalar(
        "SELECT COUNT(*) FROM (
           SELECT 1 FROM memory_relations
            WHERE valid_to IS NOT NULL AND valid_to <= valid_from
            LIMIT $1
         ) sub",
    )
    .bind(row_cap)
    .fetch_one(pool)
    .await?;

    r.entity_supersede_cycles = sqlx::query_scalar(
        "SELECT COUNT(*) FROM (
           WITH RECURSIVE walk AS (
             SELECT id AS root, superseded_by AS next, 1 AS depth
               FROM memory_entities WHERE superseded_by IS NOT NULL
             UNION ALL
             SELECT w.root, e.superseded_by, w.depth + 1
               FROM walk w
               JOIN memory_entities e ON e.id = w.next
              WHERE w.next IS NOT NULL AND w.depth < 32
           )
           SELECT 1 FROM walk WHERE next = root LIMIT $1
         ) sub",
    )
    .bind(row_cap)
    .fetch_one(pool)
    .await?;

    r.observation_supersede_cycles = sqlx::query_scalar(
        "SELECT COUNT(*) FROM (
           WITH RECURSIVE walk AS (
             SELECT id AS root, superseded_by AS next, 1 AS depth
               FROM memory_observations WHERE superseded_by IS NOT NULL
             UNION ALL
             SELECT w.root, o.superseded_by, w.depth + 1
               FROM walk w
               JOIN memory_observations o ON o.id = w.next
              WHERE w.next IS NOT NULL AND w.depth < 32
           )
           SELECT 1 FROM walk WHERE next = root LIMIT $1
         ) sub",
    )
    .bind(row_cap)
    .fetch_one(pool)
    .await?;

    r.relation_supersede_cycles = sqlx::query_scalar(
        "SELECT COUNT(*) FROM (
           WITH RECURSIVE walk AS (
             SELECT id AS root, superseded_by AS next, 1 AS depth
               FROM memory_relations WHERE superseded_by IS NOT NULL
             UNION ALL
             SELECT w.root, rel.superseded_by, w.depth + 1
               FROM walk w
               JOIN memory_relations rel ON rel.id = w.next
              WHERE w.next IS NOT NULL AND w.depth < 32
           )
           SELECT 1 FROM walk WHERE next = root LIMIT $1
         ) sub",
    )
    .bind(row_cap)
    .fetch_one(pool)
    .await?;

    r.orphan_observations = sqlx::query_scalar(
        "SELECT COUNT(*) FROM (
           SELECT 1 FROM memory_observations o
            LEFT JOIN memory_entities e ON e.id = o.entity_id
            WHERE e.id IS NULL
            LIMIT $1
         ) sub",
    )
    .bind(row_cap)
    .fetch_one(pool)
    .await?;

    r.reflection_derived_from_missing = sqlx::query_scalar(
        "SELECT COUNT(*) FROM (
           SELECT o.id FROM memory_observations o
            WHERE o.source = 'reflection'
              AND o.derived_from IS NOT NULL
              AND NOT EXISTS (
                SELECT 1 FROM memory_observations src
                 WHERE src.id = ANY(o.derived_from)
              )
            LIMIT $1
         ) sub",
    )
    .bind(row_cap)
    .fetch_one(pool)
    .await?;

    r.stale_code_anchors = sqlx::query_scalar(
        "SELECT COUNT(*) FROM (
           SELECT a.id
             FROM memory_code_anchor a
             LEFT JOIN indexed_files f ON f.id = a.file_id
             LEFT JOIN file_chunks   c ON c.id = a.chunk_id
             LEFT JOIN code_topics   t ON t.id = a.topic_id
            WHERE (a.file_id  IS NOT NULL AND f.id IS NULL)
               OR (a.chunk_id IS NOT NULL AND c.id IS NULL)
               OR (a.topic_id IS NOT NULL AND t.id IS NULL)
            LIMIT $1
         ) sub",
    )
    .bind(row_cap)
    .fetch_one(pool)
    .await?;

    r.forget_log_dangling = sqlx::query_scalar(
        "SELECT COUNT(*) FROM (
           SELECT fl.id
             FROM memory_forget_log fl
            WHERE fl.cascade = false
              AND (
                   (fl.target_type = 'entity' AND EXISTS (
                       SELECT 1 FROM memory_entities e
                        WHERE e.id = fl.target_id AND e.valid_to IS NULL
                   ))
                OR (fl.target_type = 'observation' AND EXISTS (
                       SELECT 1 FROM memory_observations o
                        WHERE o.id = fl.target_id AND o.valid_to IS NULL
                   ))
                OR (fl.target_type = 'relation' AND EXISTS (
                       SELECT 1 FROM memory_relations rel
                        WHERE rel.id = fl.target_id AND rel.valid_to IS NULL
                   ))
              )
            LIMIT $1
         ) sub",
    )
    .bind(row_cap)
    .fetch_one(pool)
    .await?;

    Ok(r)
}

/// Persist a memory-eval invariant report into `pgmcp_metadata` so
/// daemons can surface "last successful eval" without standing up a
/// separate table. Stored as a single JSON blob keyed by
/// `memory_eval_last_report`.
pub async fn record_memory_eval_report(
    pool: &PgPool,
    report: &MemoryEvalReport,
) -> Result<(), sqlx::Error> {
    let body = serde_json::json!({
        "report": report,
        "recorded_at": chrono::Utc::now(),
    });
    sqlx::query(
        "INSERT INTO pgmcp_metadata (key, value) VALUES ('memory_eval_last_report', $1)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(body.to_string())
    .execute(pool)
    .await?;
    Ok(())
}

// ============================================================================
// Completion queries
// ============================================================================

/// List all distinct project names (for completions).
pub async fn list_project_names(pool: &PgPool) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar::<_, String>("SELECT DISTINCT name FROM projects ORDER BY name")
        .fetch_all(pool)
        .await
}

/// List all distinct languages from indexed files (for completions).
pub async fn list_languages(pool: &PgPool) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar::<_, String>("SELECT DISTINCT language FROM indexed_files ORDER BY language")
        .fetch_all(pool)
        .await
}

/// Search file paths by prefix (for completions).
pub async fn search_file_paths(
    pool: &PgPool,
    prefix: &str,
    limit: i32,
) -> Result<Vec<String>, sqlx::Error> {
    let pattern = format!("{}%", prefix);
    sqlx::query_scalar::<_, String>(
        "SELECT relative_path FROM indexed_files WHERE relative_path LIKE $1 ORDER BY relative_path LIMIT $2"
    )
    .bind(&pattern)
    .bind(limit)
    .fetch_all(pool)
    .await
}

// ============================================================================
// Git history queries
// ============================================================================

/// Upsert a git commit. Returns the commit row ID.
pub async fn upsert_git_commit(
    pool: &PgPool,
    project_id: i32,
    commit_hash: &str,
    author: &str,
    author_date: DateTime<Utc>,
    subject: &str,
    body: Option<&str>,
) -> Result<i64, sqlx::Error> {
    let row = sqlx::query_scalar::<_, i64>(
        "INSERT INTO git_commits (project_id, commit_hash, author, author_date, subject, body)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (project_id, commit_hash) DO UPDATE SET
            author = EXCLUDED.author,
            author_date = EXCLUDED.author_date,
            subject = EXCLUDED.subject,
            body = EXCLUDED.body
         RETURNING id",
    )
    .bind(project_id)
    .bind(commit_hash)
    .bind(author)
    .bind(author_date)
    .bind(subject)
    .bind(body)
    .fetch_one(pool)
    .await?;

    Ok(row)
}

/// Insert a git commit chunk with its embedding. Phase 5 C3:
/// dispatch by embedding dim to legacy `embedding` (384d) or new
/// `embedding_v2` (1024d, post-C1 schema), stamping the matching
/// `embedding_signature`. Unsupported dims surface a clear
/// configuration-error message pointing at `pgmcp embed-cutover --check`.
pub async fn insert_git_commit_chunk(
    pool: &PgPool,
    commit_id: i64,
    chunk_index: i32,
    content: &str,
    embedding: &[f32],
) -> Result<(), sqlx::Error> {
    let embedding_vec = pgvector::Vector::from(embedding.to_vec());
    match embedding.len() {
        384 => {
            sqlx::query(
                "INSERT INTO git_commit_chunks
                    (commit_id, chunk_index, content, embedding, embedding_signature)
                 VALUES ($1, $2, $3, $4, 'minilm-l6-v2')
                 ON CONFLICT (commit_id, chunk_index) DO UPDATE SET
                    content = EXCLUDED.content,
                    embedding = EXCLUDED.embedding,
                    embedding_signature = EXCLUDED.embedding_signature",
            )
            .bind(commit_id)
            .bind(chunk_index)
            .bind(content)
            .bind(embedding_vec)
            .execute(pool)
            .await?;
        }
        1024 => {
            sqlx::query(
                "INSERT INTO git_commit_chunks
                    (commit_id, chunk_index, content, embedding_v2, embedding_signature)
                 VALUES ($1, $2, $3, $4, 'bge-m3-v1')
                 ON CONFLICT (commit_id, chunk_index) DO UPDATE SET
                    content = EXCLUDED.content,
                    embedding_v2 = EXCLUDED.embedding_v2,
                    embedding_signature = EXCLUDED.embedding_signature",
            )
            .bind(commit_id)
            .bind(chunk_index)
            .bind(content)
            .bind(embedding_vec)
            .execute(pool)
            .await?;
        }
        other => {
            return Err(sqlx::Error::Protocol(format!(
                "insert_git_commit_chunk: unsupported embedding dim {other} \
                 (expected 384 for MiniLM-L6-v2 or 1024 for BGE-M3); \
                 run `pgmcp embed-cutover --check`"
            )));
        }
    }
    Ok(())
}

/// Get the last indexed git commit SHA for a project.
pub async fn get_git_last_commit(
    pool: &PgPool,
    project_id: i32,
) -> Result<Option<String>, sqlx::Error> {
    let key = format!("git_last_commit:{}", project_id);
    sqlx::query_scalar::<_, String>("SELECT value FROM pgmcp_metadata WHERE key = $1")
        .bind(&key)
        .fetch_optional(pool)
        .await
}

/// Set the last indexed git commit SHA for a project.
pub async fn set_git_last_commit(
    pool: &PgPool,
    project_id: i32,
    sha: &str,
) -> Result<(), sqlx::Error> {
    let key = format!("git_last_commit:{}", project_id);
    sqlx::query(
        "INSERT INTO pgmcp_metadata (key, value) VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(&key)
    .bind(sha)
    .execute(pool)
    .await?;
    Ok(())
}

/// Update blame metadata on file_chunks for a given file.
pub async fn update_blame_for_file(
    pool: &PgPool,
    file_id: i64,
    blame_commit: &str,
    blame_author: &str,
    blame_date: DateTime<Utc>,
    start_line: i32,
    end_line: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE file_chunks SET blame_commit = $1, blame_author = $2, blame_date = $3
         WHERE file_id = $4 AND start_line <= $6 AND end_line >= $5",
    )
    .bind(blame_commit)
    .bind(blame_author)
    .bind(blame_date)
    .bind(file_id)
    .bind(start_line)
    .bind(end_line)
    .execute(pool)
    .await?;
    Ok(())
}

/// Semantic search across git commit chunks. Phase 5 C8: signature-
/// aware column dispatch. `git_commit_chunks` gained an `embedding_v2`
/// column in C1; this function picks it based on the incoming
/// embedding's dim.
pub async fn semantic_search_commits(
    pool: &PgPool,
    embedding: &[f32],
    limit: i32,
    project: Option<&str>,
    ef_search: i32,
) -> Result<Vec<CommitSearchResult>, sqlx::Error> {
    let embedding_vec = pgvector::Vector::from(embedding.to_vec());

    let col = match embedding.len() {
        384 => "embedding",
        1024 => "embedding_v2",
        other => {
            return Err(sqlx::Error::Protocol(format!(
                "semantic_search_commits: unsupported query-embedding dim {other} \
                 (expected 384 for MiniLM or 1024 for BGE-M3). \
                 Run `pgmcp embed-cutover --check` to inspect."
            )));
        }
    };

    let mut tx = pool.begin().await?;
    sqlx::query(&format!("SET LOCAL hnsw.ef_search = {}", ef_search))
        .execute(&mut *tx)
        .await?;

    let results = if let Some(proj) = project {
        sqlx::query_as::<_, CommitSearchResult>(&format!(
            "SELECT g.commit_hash, g.author, g.author_date, g.subject,
                    cc.content as chunk_content,
                    1 - (cc.{col} <=> $1) as score,
                    p.name as project_name
             FROM git_commit_chunks cc
             JOIN git_commits g ON g.id = cc.commit_id
             JOIN projects p ON p.id = g.project_id
             WHERE p.name = $3 AND cc.{col} IS NOT NULL
             ORDER BY cc.{col} <=> $1
             LIMIT $2"
        ))
        .bind(&embedding_vec)
        .bind(limit)
        .bind(proj)
        .fetch_all(&mut *tx)
        .await?
    } else {
        sqlx::query_as::<_, CommitSearchResult>(&format!(
            "SELECT g.commit_hash, g.author, g.author_date, g.subject,
                    cc.content as chunk_content,
                    1 - (cc.{col} <=> $1) as score,
                    p.name as project_name
             FROM git_commit_chunks cc
             JOIN git_commits g ON g.id = cc.commit_id
             JOIN projects p ON p.id = g.project_id
             WHERE cc.{col} IS NOT NULL
             ORDER BY cc.{col} <=> $1
             LIMIT $2"
        ))
        .bind(&embedding_vec)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await?
    };

    tx.commit().await?;
    Ok(results)
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct CommitSearchResult {
    pub commit_hash: String,
    pub author: String,
    pub author_date: DateTime<Utc>,
    pub subject: String,
    pub chunk_content: String,
    pub score: Option<f64>,
    pub project_name: String,
}

/// Get the file_id for a given absolute path.
pub async fn get_file_id_by_path(pool: &PgPool, path: &str) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar::<_, i64>("SELECT id FROM indexed_files WHERE path = $1")
        .bind(path)
        .fetch_optional(pool)
        .await
}

/// Get all projects that have git history indexing enabled (via .pgmcp.toml).
/// Returns (project_id, project_path) pairs.
pub async fn get_git_enabled_projects(pool: &PgPool) -> Result<Vec<(i32, String)>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (i32, String)>("SELECT id, path FROM projects ORDER BY name")
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Delete all projects whose `workspace_path` matches the given path.
/// CASCADE handles `indexed_files` → `file_chunks`, `git_commits` → `git_commit_chunks`.
/// Returns the number of projects deleted.
pub async fn delete_projects_by_workspace(
    pool: &PgPool,
    workspace_path: &str,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM projects WHERE workspace_path = $1")
        .bind(workspace_path)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

/// Delete projects that have zero indexed files (orphaned after stale file removal).
pub async fn cleanup_orphaned_projects(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "DELETE FROM projects p
         WHERE NOT EXISTS (SELECT 1 FROM indexed_files f WHERE f.project_id = p.id)",
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

// ============================================================================
// Cross-project similarity queries
// ============================================================================

/// Resolved file reference (from `project:relative_path` or absolute path).
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct FileReference {
    pub file_id: i64,
    pub path: String,
    pub relative_path: String,
    pub language: String,
    pub line_count: i32,
    pub project_id: i32,
    pub project_name: String,
}

/// Resolve a file reference. Supports `project:relative_path` syntax or absolute paths.
pub async fn resolve_file_reference(
    pool: &PgPool,
    file_ref: &str,
) -> Result<Option<FileReference>, sqlx::Error> {
    if let Some((project, rel_path)) = file_ref.split_once(':') {
        sqlx::query_as::<_, FileReference>(
            "SELECT f.id as file_id, f.path, f.relative_path, f.language, f.line_count,
                    p.id as project_id, p.name as project_name
             FROM indexed_files f
             JOIN projects p ON p.id = f.project_id
             WHERE p.name = $1 AND f.relative_path = $2",
        )
        .bind(project)
        .bind(rel_path)
        .fetch_optional(pool)
        .await
    } else {
        sqlx::query_as::<_, FileReference>(
            "SELECT f.id as file_id, f.path, f.relative_path, f.language, f.line_count,
                    p.id as project_id, p.name as project_name
             FROM indexed_files f
             JOIN projects p ON p.id = f.project_id
             WHERE f.path = $1",
        )
        .bind(file_ref)
        .fetch_optional(pool)
        .await
    }
}

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

    let results = sqlx::query_as::<_, ChunkPairSimilarity>(
        "SELECT ca.id as chunk_id_a, ca.content as content_a,
                ca.start_line as start_line_a, ca.end_line as end_line_a,
                cb.id as chunk_id_b, cb.content as content_b,
                cb.start_line as start_line_b, cb.end_line as end_line_b,
                1 - (ca.embedding <=> cb.embedding) as similarity
         FROM file_chunks ca
         CROSS JOIN file_chunks cb
         WHERE ca.file_id = $1 AND cb.file_id = $2
         ORDER BY similarity DESC",
    )
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

    let results = sqlx::query_as::<_, ChunkPairSimilarity>(
        "SELECT ca.id AS chunk_id_a, ca.content AS content_a,
                ca.start_line AS start_line_a, ca.end_line AS end_line_a,
                cb.id AS chunk_id_b, cb.content AS content_b,
                cb.start_line AS start_line_b, cb.end_line AS end_line_b,
                1 - (ca.embedding <=> cb.embedding) AS similarity
         FROM file_chunks ca
         JOIN file_chunks cb
              ON ca.file_id = cb.file_id AND ca.id < cb.id
         WHERE ca.file_id = $1
           AND 1 - (ca.embedding <=> cb.embedding) >= $2
         ORDER BY similarity DESC",
    )
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

    // Worktree-awareness: skip pairs whose two projects are different
    // worktrees / sibling clones of the same upstream repo (same
    // git_common_dir OR same git_root_commits). Otherwise the
    // materialized similarity table fills with same-code-different-branch
    // false positives that drown out genuine cross-repo refactor candidates.
    // See plan: ~/.claude/plans/thoroughly-examine-home-dylon-workspace-melodic-cake.md
    let results = sqlx::query_as::<_, SimilarityNeighborRow>(
        "WITH batch AS (
            SELECT c.id, c.file_id, c.embedding, f.project_id, f.path, f.language,
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
                   1 - (c2.embedding <=> b.embedding) as similarity
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
            ORDER BY c2.embedding <=> b.embedding
            LIMIT $3
        ) nn
        WHERE nn.similarity >= $4",
    )
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

    let rows = sqlx::query_as::<_, GrepChunkResult>(&sql)
        .bind(pattern)
        .bind(project)
        .bind(language)
        .bind(like_pattern)
        .bind(dedupe_worktrees)
        .bind(limit)
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

/// Information about a canonical file row used to drive cross-path
/// dedup decisions in `embed::pool`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CanonicalFileMatch {
    pub id: i64,
    pub path: String,
    pub content_hash: Option<i64>,
}

/// Look up the canonical row (i.e. `duplicate_of_file_id IS NULL`) for
/// a given `(project_id, content_hash)` pair. Used by the index-time
/// dedup decision: if a row already exists with this content elsewhere
/// in the project, the new path becomes either a rename (old path gone)
/// or a duplicate (old path still on disk).
pub async fn find_canonical_by_content_hash(
    pool: &PgPool,
    project_id: i32,
    content_hash: i64,
) -> Result<Option<CanonicalFileMatch>, sqlx::Error> {
    sqlx::query_as::<_, CanonicalFileMatch>(
        "SELECT id, path, content_hash
         FROM indexed_files
         WHERE project_id = $1
           AND content_hash = $2
           AND duplicate_of_file_id IS NULL
         ORDER BY id ASC
         LIMIT 1",
    )
    .bind(project_id)
    .bind(content_hash)
    .fetch_optional(pool)
    .await
}

/// Update a canonical row's path in place. Used when the rename
/// detection sees the previous path is gone on disk. Chunks are NOT
/// touched.
pub async fn update_file_path_in_place(
    pool: &PgPool,
    file_id: i64,
    new_path: &str,
    new_relative_path: &str,
    modified_at: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE indexed_files
         SET path = $2, relative_path = $3, modified_at = $4, indexed_at = NOW()
         WHERE id = $1",
    )
    .bind(file_id)
    .bind(new_path)
    .bind(new_relative_path)
    .bind(modified_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// Insert a duplicate-pointer row referencing an existing canonical.
/// The new row carries metadata (path/relative_path/size/language/etc.)
/// but no chunks of its own — queries follow `duplicate_of_file_id` to
/// the canonical via `COALESCE`. Returns the new row's id.
pub async fn insert_duplicate_file(
    pool: &PgPool,
    project_id: i32,
    path: &str,
    relative_path: &str,
    language: &str,
    size_bytes: i64,
    content_hash: i64,
    canonical_file_id: i64,
    modified_at: DateTime<Utc>,
) -> Result<i64, sqlx::Error> {
    let id: (i64,) = sqlx::query_as(
        "INSERT INTO indexed_files (
            project_id, path, relative_path, language, size_bytes,
            content, content_hash, line_count, truncated, modified_at,
            duplicate_of_file_id
         )
         VALUES ($1, $2, $3, $4, $5, NULL, $6, 0, false, $7, $8)
         ON CONFLICT (path) DO UPDATE SET
            project_id = EXCLUDED.project_id,
            relative_path = EXCLUDED.relative_path,
            language = EXCLUDED.language,
            size_bytes = EXCLUDED.size_bytes,
            content_hash = EXCLUDED.content_hash,
            modified_at = EXCLUDED.modified_at,
            duplicate_of_file_id = EXCLUDED.duplicate_of_file_id,
            indexed_at = NOW()
         RETURNING id",
    )
    .bind(project_id)
    .bind(path)
    .bind(relative_path)
    .bind(language)
    .bind(size_bytes)
    .bind(content_hash)
    .bind(modified_at)
    .bind(canonical_file_id)
    .fetch_one(pool)
    .await?;
    Ok(id.0)
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
        "SELECT cta.chunk_id, ct.id AS topic_id, ct.label, ct.keywords, cta.membership_score
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

/// One row per file with the count of distinct source files importing it.
/// Used by `extraction_candidates` to estimate `effort.call_sites_to_update`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CallSiteCount {
    pub file_id: i64,
    pub importer_count: i64,
    /// Approximate count of unresolved imports (target_raw match by basename).
    /// Filled when the symbol_references table is empty AND the language has
    /// no resolved-import support (Go/Java/C/C++ pre-Tier-0e).
    pub unresolved_count: i64,
}

/// For each input file_id, count the distinct files that import it.
/// Resolved-only path: joins `code_graph_edges` where `target_file_id`
/// already resolved at graph-analysis time. Unresolved-target imports
/// (Go/Java/C/C++) are reported as `unresolved_count` via fuzzy basename
/// match — it's an upper bound, hence the dedicated field.
pub async fn count_call_sites_to_files(
    pool: &PgPool,
    file_ids: &[i64],
) -> Result<Vec<CallSiteCount>, sqlx::Error> {
    if file_ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as::<_, CallSiteCount>(
        "WITH targets AS (
            SELECT id, regexp_replace(relative_path, '^.*/', '') AS basename
            FROM indexed_files
            WHERE id = ANY($1)
         ),
         resolved AS (
            SELECT t.id AS file_id,
                   COUNT(DISTINCT cge.source_file_id) AS importer_count
            FROM targets t
            LEFT JOIN code_graph_edges cge
                  ON cge.target_file_id = t.id AND cge.edge_type = 'import'
            GROUP BY t.id
         ),
         unresolved AS (
            SELECT t.id AS file_id,
                   COUNT(DISTINCT cge.source_file_id) AS unresolved_count
            FROM targets t
            LEFT JOIN code_graph_edges cge
                  ON cge.target_file_id IS NULL
                 AND cge.edge_type = 'import'
                 AND cge.target_raw ILIKE '%' || regexp_replace(t.basename, '\\.[^.]+$', '') || '%'
            GROUP BY t.id
         )
         SELECT r.file_id,
                COALESCE(r.importer_count, 0) AS importer_count,
                COALESCE(u.unresolved_count, 0) AS unresolved_count
         FROM resolved r
         LEFT JOIN unresolved u ON u.file_id = r.file_id",
    )
    .bind(file_ids)
    .fetch_all(pool)
    .await
}

/// Subset of `file_metrics` columns used for risk-tier classification in
/// `extraction_candidates`. Returns one row per requested file_id; files
/// without a `file_metrics` row are simply absent.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FileRiskMetrics {
    pub file_id: i64,
    pub pagerank: Option<f64>,
    pub churn_rate: Option<f64>,
    pub fix_commit_ratio: Option<f64>,
    pub days_since_last_change: Option<i32>,
}

/// Pull churn / pagerank / fix-ratio for a set of file_ids in one query.
pub async fn get_file_risk_metrics(
    pool: &PgPool,
    file_ids: &[i64],
) -> Result<Vec<FileRiskMetrics>, sqlx::Error> {
    if file_ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as::<_, FileRiskMetrics>(
        "SELECT file_id, pagerank, churn_rate, fix_commit_ratio, days_since_last_change
         FROM file_metrics
         WHERE file_id = ANY($1)",
    )
    .bind(file_ids)
    .fetch_all(pool)
    .await
}

/// One row per zombie-candidate file: low PageRank percentile, low in-degree,
/// long-idle. Used by `stale_zombie_detector`.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct ZombieCandidate {
    pub file_id: i64,
    pub relative_path: String,
    pub line_count: i32,
    pub pagerank: Option<f64>,
    pub pagerank_pct: f64,
    pub in_degree: Option<i32>,
    pub author_count: Option<i32>,
    pub commit_count: Option<i32>,
    pub days_since_last_change: Option<i32>,
}

/// Find files that are graph + history "zombies": low PageRank, low in-degree,
/// long-idle. Distinct from `find_orphans` (topic-based) — this combines
/// graph centrality, import topology, and authorial abandonment.
pub async fn find_zombie_candidates(
    pool: &PgPool,
    project_name: &str,
    min_days_idle: i32,
    max_pagerank_pct: f64,
    limit: i32,
) -> Result<Vec<ZombieCandidate>, sqlx::Error> {
    sqlx::query_as::<_, ZombieCandidate>(
        "WITH ranked AS (
            SELECT f.id AS file_id,
                   f.relative_path,
                   f.line_count,
                   fm.pagerank,
                   fm.in_degree,
                   fm.author_count,
                   fm.commit_count,
                   fm.days_since_last_change,
                   PERCENT_RANK() OVER (ORDER BY COALESCE(fm.pagerank, 0)) AS pagerank_pct
            FROM indexed_files f
            JOIN projects p ON p.id = f.project_id
            LEFT JOIN file_metrics fm ON fm.file_id = f.id
            WHERE p.name = $1
         )
         SELECT file_id, relative_path, line_count, pagerank, pagerank_pct,
                in_degree, author_count, commit_count, days_since_last_change
         FROM ranked
         WHERE COALESCE(in_degree, 0) <= 1
           AND COALESCE(days_since_last_change, 0) > $2
           AND pagerank_pct <= $3
         ORDER BY pagerank_pct ASC,
                  COALESCE(days_since_last_change, 0) DESC,
                  file_id ASC
         LIMIT $4",
    )
    .bind(project_name)
    .bind(min_days_idle)
    .bind(max_pagerank_pct)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// One row per chunk in a god-candidate file, with its dominant FCM topic
/// (highest membership_score) when one is known. Used by
/// `recommend_module_split` to group chunks into proposed sub-files.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct GodFileChunkRow {
    pub file_id: i64,
    pub relative_path: String,
    pub language: String,
    pub line_count: i32,
    pub chunk_id: i64,
    pub chunk_index: i32,
    pub start_line: i32,
    pub end_line: i32,
    pub topic_id: Option<i64>,
    pub topic_label: Option<String>,
    pub topic_keywords: Option<Vec<String>>,
    pub membership_score: Option<f64>,
}

/// For a project, return all chunks of files whose `line_count >= min_lines`,
/// each annotated with the chunk's dominant FCM topic (the assignment row
/// with the highest `membership_score`). Topics may be NULL when no FCM run
/// has reached that chunk yet.
///
/// Drives `recommend_module_split` — chunks of a god file get grouped by
/// `topic_id` to produce per-topic sub-file recommendations.
pub async fn get_god_file_chunks_with_topics(
    pool: &PgPool,
    project_name: &str,
    min_lines: i32,
) -> Result<Vec<GodFileChunkRow>, sqlx::Error> {
    sqlx::query_as::<_, GodFileChunkRow>(
        "WITH god_files AS (
            SELECT f.id, f.relative_path, f.language, f.line_count
            FROM indexed_files f
            JOIN projects p ON p.id = f.project_id
            WHERE p.name = $1 AND f.line_count >= $2
         ),
         dominant_topic AS (
            SELECT DISTINCT ON (cta.chunk_id)
                   cta.chunk_id,
                   cta.topic_id,
                   cta.membership_score,
                   ct.label,
                   ct.keywords
            FROM chunk_topic_assignments cta
            JOIN code_topics ct ON ct.id = cta.topic_id
            ORDER BY cta.chunk_id, cta.membership_score DESC, cta.topic_id ASC
         )
         SELECT g.id AS file_id,
                g.relative_path,
                g.language,
                g.line_count,
                fc.id AS chunk_id,
                fc.chunk_index,
                fc.start_line,
                fc.end_line,
                dt.topic_id,
                dt.label AS topic_label,
                dt.keywords AS topic_keywords,
                dt.membership_score
         FROM god_files g
         JOIN file_chunks fc ON fc.file_id = g.id
         LEFT JOIN dominant_topic dt ON dt.chunk_id = fc.id
         ORDER BY g.id, fc.chunk_index",
    )
    .bind(project_name)
    .bind(min_lines)
    .fetch_all(pool)
    .await
}

// ============================================================================
// Tier 4 — engineer/architect workflow queries
// ============================================================================

/// One row per "hot path" file: high PageRank, high churn, high fix-commit ratio.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct HotPathRow {
    pub file_id: i64,
    pub relative_path: String,
    pub pagerank: Option<f64>,
    pub churn_rate: Option<f64>,
    pub fix_commit_ratio: Option<f64>,
    pub bug_proneness: Option<f64>,
    pub instability: Option<f64>,
    pub in_degree: Option<i32>,
    pub author_count: Option<i32>,
    pub commit_count: Option<i32>,
    pub pagerank_pct: f64,
    pub churn_pct: f64,
    pub fix_ratio_pct: f64,
}

/// Files in the intersection of top-P% PageRank, top-P% churn, and top-P%
/// fix_commit_ratio for a project. Used by `hot_path_audit`.
pub async fn find_hot_paths(
    pool: &PgPool,
    project_name: &str,
    percentile_threshold: f64,
    limit: i32,
) -> Result<Vec<HotPathRow>, sqlx::Error> {
    sqlx::query_as::<_, HotPathRow>(
        "WITH stats AS (
            SELECT f.id AS file_id,
                   f.relative_path,
                   fm.pagerank,
                   fm.churn_rate,
                   fm.fix_commit_ratio,
                   fm.bug_proneness,
                   fm.instability,
                   fm.in_degree,
                   fm.author_count,
                   fm.commit_count,
                   PERCENT_RANK() OVER (ORDER BY COALESCE(fm.pagerank, 0)) AS pagerank_pct,
                   PERCENT_RANK() OVER (ORDER BY COALESCE(fm.churn_rate, 0)) AS churn_pct,
                   PERCENT_RANK() OVER (ORDER BY COALESCE(fm.fix_commit_ratio, 0)) AS fix_ratio_pct
            FROM indexed_files f
            JOIN projects p ON p.id = f.project_id
            LEFT JOIN file_metrics fm ON fm.file_id = f.id
            WHERE p.name = $1
         )
         SELECT file_id, relative_path,
                pagerank, churn_rate, fix_commit_ratio,
                bug_proneness, instability,
                in_degree, author_count, commit_count,
                pagerank_pct, churn_pct, fix_ratio_pct
         FROM stats
         WHERE pagerank_pct >= $2
           AND churn_pct >= $2
           AND fix_ratio_pct >= $2
         ORDER BY (pagerank_pct + churn_pct + fix_ratio_pct) DESC,
                  file_id ASC
         LIMIT $3",
    )
    .bind(project_name)
    .bind(percentile_threshold)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// One row per file with its top author (by lines blamed) and risk score.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct BusFactorRow {
    pub file_id: i64,
    pub relative_path: String,
    pub top_author: String,
    pub top_share: f64,
    pub distinct_authors: i64,
    pub last_touch: Option<DateTime<Utc>>,
    pub pagerank: Option<f64>,
    pub risk_score: Option<f64>,
}

/// Per-file bus-factor risk for a project: top author's share of blamed lines
/// × pagerank ÷ author count. Used by `bus_factor_map`.
pub async fn find_bus_factor_files(
    pool: &PgPool,
    project_name: &str,
    min_pagerank_pct: f64,
    limit: i32,
) -> Result<Vec<BusFactorRow>, sqlx::Error> {
    sqlx::query_as::<_, BusFactorRow>(
        "WITH per_file AS (
            SELECT f.id,
                   f.relative_path,
                   fc.blame_author,
                   COUNT(*) AS lines_blamed,
                   MAX(fc.blame_date) AS last_touch
            FROM file_chunks fc
            JOIN indexed_files f ON f.id = fc.file_id
            JOIN projects p ON p.id = f.project_id
            WHERE p.name = $1 AND fc.blame_author IS NOT NULL
            GROUP BY f.id, f.relative_path, fc.blame_author
         ),
         top AS (
            SELECT id,
                   relative_path,
                   (array_agg(blame_author ORDER BY lines_blamed DESC))[1] AS top_author,
                   (MAX(lines_blamed)::float8) /
                       NULLIF(SUM(lines_blamed)::float8, 0) AS top_share,
                   COUNT(*)::bigint AS distinct_authors,
                   MAX(last_touch) AS last_touch
            FROM per_file
            GROUP BY id, relative_path
         ),
         ranked AS (
            SELECT t.*,
                   fm.pagerank,
                   PERCENT_RANK() OVER (ORDER BY COALESCE(fm.pagerank, 0)) AS pr_pct
            FROM top t
            LEFT JOIN file_metrics fm ON fm.file_id = t.id
         )
         SELECT id AS file_id,
                relative_path,
                top_author,
                top_share,
                distinct_authors,
                last_touch,
                pagerank,
                (COALESCE(pagerank, 0.0) * top_share /
                    GREATEST(1.0, distinct_authors::float8)) AS risk_score
         FROM ranked
         WHERE pr_pct >= $2
         ORDER BY risk_score DESC NULLS LAST, file_id ASC
         LIMIT $3",
    )
    .bind(project_name)
    .bind(min_pagerank_pct)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// One row per (file, top_author) pair within the recency window. Used by
/// `reviewer_recommender` to aggregate per-author file ownership.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FileAuthorRow {
    pub relative_path: String,
    pub top_author: Option<String>,
    pub last_touch_days: Option<i32>,
}

/// For each requested file, return the dominant blame_author within the
/// recency window. Files without blame coverage return `top_author = NULL`.
pub async fn find_dominant_authors_for_files(
    pool: &PgPool,
    project_name: &str,
    file_paths: &[String],
    recency_window_days: i32,
) -> Result<Vec<FileAuthorRow>, sqlx::Error> {
    if file_paths.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as::<_, FileAuthorRow>(
        "WITH per_file AS (
            SELECT f.relative_path,
                   fc.blame_author,
                   COUNT(*) AS lines_blamed,
                   MAX(fc.blame_date) AS last_touch
            FROM file_chunks fc
            JOIN indexed_files f ON f.id = fc.file_id
            JOIN projects p ON p.id = f.project_id
            WHERE p.name = $1
              AND f.relative_path = ANY($2)
              AND fc.blame_author IS NOT NULL
              AND fc.blame_date >= NOW() - ($3 || ' days')::interval
            GROUP BY f.relative_path, fc.blame_author
         )
         SELECT relative_path,
                (array_agg(blame_author ORDER BY lines_blamed DESC))[1] AS top_author,
                EXTRACT(DAY FROM (NOW() - MAX(last_touch)))::int AS last_touch_days
         FROM per_file
         GROUP BY relative_path",
    )
    .bind(project_name)
    .bind(file_paths)
    .bind(recency_window_days)
    .fetch_all(pool)
    .await
}

// ============================================================================
// Tier 5 — audit & trend queries
// ============================================================================

/// One row per unresolved external dep target. Used by `dependency_health`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UnresolvedDepRow {
    pub target_raw: String,
    pub importer_count: i64,
    pub usage_centrality: f64,
    pub latest_change_days: Option<f64>,
    pub sample_importers: Vec<String>,
}

/// External dependency-target audit. Groups `code_graph_edges` rows where
/// `target_file_id IS NULL` (unresolved imports — typically external crates,
/// system libraries, or Go/Java/C/C++ targets pre-Tier-0e) by `target_raw`.
pub async fn find_unresolved_dependencies(
    pool: &PgPool,
    project_id: Option<i32>,
    limit: i32,
) -> Result<Vec<UnresolvedDepRow>, sqlx::Error> {
    sqlx::query_as::<_, UnresolvedDepRow>(
        "SELECT cge.target_raw,
                COUNT(DISTINCT cge.source_file_id) AS importer_count,
                COALESCE(SUM(COALESCE(fm.pagerank, 0.0)), 0.0) AS usage_centrality,
                EXTRACT(EPOCH FROM (NOW() - MAX(f.indexed_at)))/86400.0 AS latest_change_days,
                (array_agg(DISTINCT f.relative_path))[1:5] AS sample_importers
         FROM code_graph_edges cge
         JOIN indexed_files f ON f.id = cge.source_file_id
         LEFT JOIN file_metrics fm ON fm.file_id = cge.source_file_id
         WHERE cge.target_file_id IS NULL
           AND cge.target_raw IS NOT NULL
           AND cge.edge_type = 'import'
           AND ($1::int IS NULL OR cge.project_id = $1)
         GROUP BY cge.target_raw
         ORDER BY usage_centrality DESC NULLS LAST, importer_count DESC, target_raw ASC
         LIMIT $2",
    )
    .bind(project_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// One row per file in the merge-conflict scan. Used by `merge_conflict_risk`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MergeRiskRow {
    pub file_path: String,
    pub recent_commits: i64,
    pub distinct_recent_authors: i64,
    pub top_other_authors: Vec<String>,
}

/// Find files in `branch_files` with overlapping recent commits from other
/// authors. Used by `merge_conflict_risk`. The `exclude_email` is omitted
/// from the per-file partner counts.
pub async fn find_merge_conflict_risks(
    pool: &PgPool,
    project_name: &str,
    branch_files: &[String],
    window_days: i32,
    exclude_email: Option<&str>,
) -> Result<Vec<MergeRiskRow>, sqlx::Error> {
    if branch_files.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as::<_, MergeRiskRow>(
        "WITH commits_in_window AS (
            SELECT gc.author, gcf.file_path
            FROM git_commits gc
            JOIN git_commit_files gcf ON gcf.commit_id = gc.id
            JOIN projects p ON p.id = gc.project_id
            WHERE p.name = $1
              AND gc.author_date >= NOW() - ($3 || ' days')::interval
              AND gcf.file_path = ANY($2)
              AND ($4::text IS NULL OR gc.author <> $4)
         )
         SELECT file_path,
                COUNT(*)::bigint AS recent_commits,
                COUNT(DISTINCT author)::bigint AS distinct_recent_authors,
                (array_agg(DISTINCT author))[1:5] AS top_other_authors
         FROM commits_in_window
         GROUP BY file_path
         ORDER BY distinct_recent_authors DESC, recent_commits DESC, file_path ASC",
    )
    .bind(project_name)
    .bind(branch_files)
    .bind(window_days)
    .bind(exclude_email)
    .fetch_all(pool)
    .await
}

/// One time-bucket row for a project (or single file). Used by `module_growth_trajectory`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct GrowthBucketRow {
    pub period_start: DateTime<Utc>,
    pub commits: i64,
    pub authors: i64,
    pub additions: Option<i64>,
    pub deletions: Option<i64>,
}

/// Bucket commits into time periods (week/month/quarter) and aggregate.
/// `interval_unit` is one of "week", "month", "quarter" — caller validates.
pub async fn get_growth_buckets(
    pool: &PgPool,
    project_name: &str,
    file_path: Option<&str>,
    interval_unit: &str,
    lookback_buckets: i32,
) -> Result<Vec<GrowthBucketRow>, sqlx::Error> {
    sqlx::query_as::<_, GrowthBucketRow>(
        // We can't bind interval keywords directly; concat-and-cast is the
        // typical workaround. interval_unit is hard-coded to one of three
        // strings by the caller, so injection isn't a concern.
        &format!(
            "WITH per_commit AS (
                SELECT gc.author_date,
                       gc.author,
                       gc.id,
                       date_trunc('{unit}', gc.author_date) AS bucket
                FROM git_commits gc
                JOIN projects p ON p.id = gc.project_id
                LEFT JOIN git_commit_files gcf ON gcf.commit_id = gc.id
                WHERE p.name = $1
                  AND gc.author_date >= NOW() - ($3 * INTERVAL '1 {unit}')
                  AND ($2::text IS NULL OR gcf.file_path = $2)
                GROUP BY gc.id, gc.author_date, gc.author
             )
             SELECT bucket AS period_start,
                    COUNT(*)::bigint AS commits,
                    COUNT(DISTINCT author)::bigint AS authors,
                    NULL::bigint AS additions,
                    NULL::bigint AS deletions
             FROM per_commit
             GROUP BY bucket
             ORDER BY bucket ASC",
            unit = interval_unit
        ),
    )
    .bind(project_name)
    .bind(file_path)
    .bind(lookback_buckets)
    .fetch_all(pool)
    .await
}

/// Resolve file_id to its line_count (for estimating shared lines).
pub async fn get_file_line_count(pool: &PgPool, file_id: i64) -> Result<i32, sqlx::Error> {
    sqlx::query_scalar::<_, i32>("SELECT line_count FROM indexed_files WHERE id = $1")
        .bind(file_id)
        .fetch_one(pool)
        .await
}

/// Find files matching a module path pattern within a project.
pub async fn find_files_by_path_pattern(
    pool: &PgPool,
    project: &str,
    pattern: &str,
) -> Result<Vec<FileReference>, sqlx::Error> {
    let like_pattern = format!("%{}%", pattern);
    sqlx::query_as::<_, FileReference>(
        "SELECT f.id as file_id, f.path, f.relative_path, f.language, f.line_count,
                p.id as project_id, p.name as project_name
         FROM indexed_files f
         JOIN projects p ON p.id = f.project_id
         WHERE p.name = $1 AND f.relative_path LIKE $2
         ORDER BY f.relative_path",
    )
    .bind(project)
    .bind(&like_pattern)
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
        let rows = sqlx::query_as::<_, BulkChunkRow>(&format!(
            "SELECT c.id as chunk_id, c.file_id, f.project_id, p.name as project_name,
                    f.path, f.language, c.content, c.{col}::real[] as embedding
             FROM file_chunks c
             JOIN indexed_files f ON f.id = c.file_id
             JOIN projects p ON p.id = f.project_id
             WHERE f.language = $1
               AND c.{col} IS NOT NULL
               AND {canonical_filter}
             ORDER BY c.id",
        ))
        .bind(lang)
        .fetch_all(pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    } else {
        let rows = sqlx::query_as::<_, BulkChunkRow>(&format!(
            "SELECT c.id as chunk_id, c.file_id, f.project_id, p.name as project_name,
                    f.path, f.language, c.content, c.{col}::real[] as embedding
             FROM file_chunks c
             JOIN indexed_files f ON f.id = c.file_id
             JOIN projects p ON p.id = f.project_id
             WHERE c.{col} IS NOT NULL
               AND {canonical_filter}
             ORDER BY c.id",
        ))
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
        let rows = sqlx::query_as::<_, BulkChunkRow>(&format!(
            "SELECT c.id as chunk_id, c.file_id, f.project_id, p.name as project_name,
                    f.path, f.language, c.content, c.{col}::real[] as embedding
             FROM file_chunks c
             JOIN indexed_files f ON f.id = c.file_id
             JOIN projects p ON p.id = f.project_id
             WHERE p.name = $1 AND f.language = $2
               AND c.{col} IS NOT NULL
             ORDER BY c.id"
        ))
        .bind(project_name)
        .bind(lang)
        .fetch_all(pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    } else {
        let rows = sqlx::query_as::<_, BulkChunkRow>(&format!(
            "SELECT c.id as chunk_id, c.file_id, f.project_id, p.name as project_name,
                    f.path, f.language, c.content, c.{col}::real[] as embedding
             FROM file_chunks c
             JOIN indexed_files f ON f.id = c.file_id
             JOIN projects p ON p.id = f.project_id
             WHERE p.name = $1
               AND c.{col} IS NOT NULL
             ORDER BY c.id"
        ))
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

/// Delete all topics and their assignments for a given scope.
pub async fn clear_topics_for_scope(pool: &PgPool, scope: &str) -> Result<(), sqlx::Error> {
    // Delete assignments first (FK constraint)
    sqlx::query(
        "DELETE FROM chunk_topic_assignments WHERE topic_id IN (
            SELECT id FROM code_topics WHERE scope = $1
        )",
    )
    .bind(scope)
    .execute(pool)
    .await?;

    sqlx::query("DELETE FROM code_topics WHERE scope = $1")
        .bind(scope)
        .execute(pool)
        .await?;

    Ok(())
}

/// Store discovered topics and their chunk assignments in the DB.
pub async fn store_topics(
    pool: &PgPool,
    scope: &str,
    topics: &[crate::cron::topic_clustering::TopicResult],
) -> Result<(), sqlx::Error> {
    // Per-topic transaction: each topic's `code_topics` row + its chunk
    // assignments commit together, or roll back together. A failed topic
    // does NOT abort the whole `store_topics` call — we log and continue
    // so a single transient FK conflict doesn't lose the rest of a
    // 200-topic clustering run.
    //
    // FK-resilience: between when topic clustering starts (12+ minutes
    // ago for a 178k-chunk corpus) and when assignments are inserted
    // here, the daemon's reindex/file watcher may have deleted some
    // `file_chunks` rows. The `chunk_topic_assignments.chunk_id ->
    // file_chunks.id` FK rejects those inserts. We sidestep this with a
    // bulk `INSERT ... SELECT ... WHERE EXISTS` that silently skips
    // orphaned chunk_ids — preserving the topics that *can* still be
    // recorded while dropping the rows that no longer have a valid
    // parent. (Bug 1 in
    // ~/.claude/plans/thoroughly-examine-home-dylon-workspace-melodic-cake.md.)
    let mut errors: Vec<(i32, sqlx::Error)> = Vec::new();
    for topic in topics {
        let top_files_json =
            serde_json::to_value(&topic.top_files).unwrap_or(serde_json::Value::Null);

        // Convert keyword_scores from f64 to f32 for REAL[] column
        let keyword_scores_f32: Vec<f32> = topic.keyword_scores.iter().map(|&s| s as f32).collect();

        // Phase 7: persist centroid as REAL[] for warm-start (if non-empty).
        let centroid_opt: Option<&[f32]> = if topic.centroid.is_empty() {
            None
        } else {
            Some(&topic.centroid)
        };
        // Phase 9: persist parent_topic_ids for hierarchy rows (BIGINT[]).
        let parent_ids_opt: Option<&[i64]> = if topic.parent_topic_ids.is_empty() {
            None
        } else {
            Some(&topic.parent_topic_ids)
        };

        let mut tx = pool.begin().await?;

        // Validate `representative_chunk_id` exists at INSERT time. The
        // `code_topics.representative_chunk_id` FK rejects nonexistent IDs
        // even though the column is nullable with `ON DELETE SET NULL` —
        // the `ON DELETE` only fires when the parent is deleted *after* a
        // valid INSERT. We use a sub-SELECT that returns NULL when the
        // chunk doesn't exist; that NULL satisfies the FK trivially. The
        // `FOR KEY SHARE` prevents the row from being deleted between
        // the validation SELECT and the INSERT's FK trigger fire.
        let topic_id_res = sqlx::query_scalar::<_, i32>(
            "INSERT INTO code_topics
                (scope, cluster_index, label, chunk_count, file_count, project_count,
                 project_names, avg_internal_similarity, representative_chunk_id,
                 representative_snippet, top_files, keywords, keyword_scores,
                 centroid, parent_topic_ids)
             VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8,
                (SELECT id FROM file_chunks WHERE id = $9 FOR KEY SHARE),
                $10, $11, $12, $13, $14, $15
             )
             ON CONFLICT (scope, cluster_index) DO UPDATE SET
                label = EXCLUDED.label,
                chunk_count = EXCLUDED.chunk_count,
                file_count = EXCLUDED.file_count,
                project_count = EXCLUDED.project_count,
                project_names = EXCLUDED.project_names,
                avg_internal_similarity = EXCLUDED.avg_internal_similarity,
                representative_chunk_id = EXCLUDED.representative_chunk_id,
                representative_snippet = EXCLUDED.representative_snippet,
                top_files = EXCLUDED.top_files,
                keywords = EXCLUDED.keywords,
                keyword_scores = EXCLUDED.keyword_scores,
                centroid = COALESCE(EXCLUDED.centroid, code_topics.centroid),
                parent_topic_ids = COALESCE(EXCLUDED.parent_topic_ids, code_topics.parent_topic_ids),
                computed_at = NOW()
             RETURNING id"
        )
        .bind(scope)
        .bind(topic.cluster_index)
        .bind(&topic.label)
        .bind(topic.chunk_ids.len() as i32)
        .bind(topic.file_ids.len() as i32)
        .bind(topic.project_names.len() as i32)
        .bind(&topic.project_names)
        .bind(topic.avg_internal_similarity)
        .bind(topic.representative_chunk_id)
        .bind(&topic.representative_snippet)
        .bind(&top_files_json)
        .bind(&topic.keywords)
        .bind(&keyword_scores_f32)
        .bind(centroid_opt)
        .bind(parent_ids_opt)
        .fetch_one(&mut *tx)
        .await;

        let topic_id = match topic_id_res {
            Ok(id) => id,
            Err(e) => {
                let _ = tx.rollback().await;
                errors.push((topic.cluster_index, e));
                continue;
            }
        };

        // Bulk-insert assignments. Use a CTE that locks the parent
        // `file_chunks` rows with `FOR KEY SHARE` *before* the INSERT
        // runs — this fixes the TOCTOU race the previous `WHERE EXISTS`
        // version had, where a concurrent DELETE between the EXISTS
        // check and the FK trigger fire would still cause the FK to
        // fail. `FOR KEY SHARE` is the weakest lock that blocks DELETE
        // (and key-changing UPDATEs); concurrent reads + non-key UPDATEs
        // still proceed. The lock is released when this transaction
        // commits or rolls back.
        if !topic.chunk_ids.is_empty() {
            // Pad memberships with 1.0 if the FCM result didn't supply one
            // for every chunk (defensive — should never happen).
            let n = topic.chunk_ids.len();
            let mut memberships: Vec<f64> = topic
                .memberships
                .iter()
                .copied()
                .chain(std::iter::repeat(1.0))
                .take(n)
                .collect();
            memberships.truncate(n);

            let assign_res = sqlx::query(
                "WITH locked AS (
                     SELECT fc.id AS chunk_id
                     FROM file_chunks fc
                     WHERE fc.id = ANY($1::bigint[])
                     FOR KEY SHARE
                 )
                 INSERT INTO chunk_topic_assignments (chunk_id, topic_id, membership_score)
                 SELECT v.chunk_id, $2, v.membership
                 FROM unnest($1::bigint[], $3::double precision[]) AS v(chunk_id, membership)
                 JOIN locked l ON l.chunk_id = v.chunk_id
                 ON CONFLICT (chunk_id, topic_id) DO UPDATE SET
                    membership_score = EXCLUDED.membership_score",
            )
            .bind(&topic.chunk_ids)
            .bind(topic_id)
            .bind(&memberships)
            .execute(&mut *tx)
            .await;

            if let Err(e) = assign_res {
                let _ = tx.rollback().await;
                errors.push((topic.cluster_index, e));
                continue;
            }
        }

        if let Err(e) = tx.commit().await {
            errors.push((topic.cluster_index, e));
        }
    }

    if !errors.is_empty() {
        // Log per-topic failures via tracing; return Ok unless ALL topics
        // failed (in which case the most-recent error is propagated).
        for (cluster_index, err) in &errors {
            tracing::warn!(
                cluster_index,
                error = %err,
                "store_topics: per-topic transaction failed (continuing)"
            );
        }
        if errors.len() == topics.len() && !topics.is_empty() {
            // All topics failed — surface the last error.
            return Err(errors.into_iter().last().expect("non-empty").1);
        }
    }

    Ok(())
}

/// Load cached topics for a given scope from the DB.
pub async fn load_cached_topics(
    pool: &PgPool,
    scope: &str,
    limit: i32,
) -> Result<Vec<serde_json::Value>, sqlx::Error> {
    let rows = sqlx::query_as::<_, CachedTopicRow>(
        "SELECT id, scope, cluster_index, label, chunk_count, file_count,
                project_count, project_names, avg_internal_similarity,
                representative_snippet, top_files, keywords, keyword_scores, computed_at
         FROM code_topics
         WHERE scope = $1
         ORDER BY chunk_count DESC
         LIMIT $2",
    )
    .bind(scope)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    let results: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|r| {
            serde_json::json!({
                "id": r.cluster_index,
                "label": r.label,
                "keywords": r.keywords,
                "keyword_scores": r.keyword_scores,
                "size": r.chunk_count,
                "files": r.file_count,
                "projects": r.project_names,
                "project_count": r.project_count,
                "avg_internal_similarity": r.avg_internal_similarity,
                "representative_snippet": r.representative_snippet,
                "representative_files": r.top_files,
                "computed_at": r.computed_at.map(|t| t.to_rfc3339()),
            })
        })
        .collect();

    Ok(results)
}

#[derive(Debug, sqlx::FromRow)]
struct CachedTopicRow {
    #[allow(dead_code)]
    id: i32,
    #[allow(dead_code)]
    scope: String,
    cluster_index: i32,
    label: String,
    chunk_count: i32,
    file_count: i32,
    project_count: i32,
    project_names: Vec<String>,
    avg_internal_similarity: Option<f64>,
    representative_snippet: Option<String>,
    top_files: Option<serde_json::Value>,
    keywords: Option<Vec<String>>,
    keyword_scores: Option<Vec<f32>>,
    computed_at: Option<DateTime<Utc>>,
}

// ============================================================================
// Git commit file tracking queries
// ============================================================================

/// Insert a file changed in a git commit.
pub async fn insert_commit_file(
    pool: &PgPool,
    commit_id: i64,
    file_path: &str,
    change_type: char,
) -> Result<(), sqlx::Error> {
    let ct = change_type.to_string();
    sqlx::query(
        "INSERT INTO git_commit_files (commit_id, file_path, change_type)
         VALUES ($1, $2, $3)
         ON CONFLICT (commit_id, file_path) DO NOTHING",
    )
    .bind(commit_id)
    .bind(file_path)
    .bind(&ct)
    .execute(pool)
    .await?;
    Ok(())
}

/// Get commits that have no entries in git_commit_files (for backfill).
/// Returns (commit_db_id, commit_hash) pairs.
pub async fn get_commits_missing_files(
    pool: &PgPool,
    project_id: i32,
) -> Result<Vec<(i64, String)>, sqlx::Error> {
    sqlx::query_as::<_, (i64, String)>(
        "SELECT gc.id, gc.commit_hash
         FROM git_commits gc
         WHERE gc.project_id = $1
           AND NOT EXISTS (
               SELECT 1 FROM git_commit_files gcf WHERE gcf.commit_id = gc.id
           )
         ORDER BY gc.id",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
}

// ============================================================================
// Analysis tool queries
// ============================================================================

/// Orphan chunk: a chunk not assigned to any topic.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct OrphanChunkRow {
    pub chunk_id: i64,
    pub content: String,
    pub path: String,
    pub language: String,
    pub project_name: String,
    pub chunk_index: i32,
}

/// Find chunks not assigned to any topic (HDBSCAN noise).
pub async fn find_orphan_chunks(
    pool: &PgPool,
    project: Option<&str>,
    language: Option<&str>,
    limit: i32,
) -> Result<Vec<OrphanChunkRow>, sqlx::Error> {
    match (project, language) {
        (Some(proj), Some(lang)) => {
            sqlx::query_as::<_, OrphanChunkRow>(
                "SELECT c.id as chunk_id, c.content, f.path, f.language,
                        p.name as project_name, c.chunk_index
                 FROM file_chunks c
                 JOIN indexed_files f ON f.id = c.file_id
                 JOIN projects p ON p.id = f.project_id
                 WHERE NOT EXISTS (
                     SELECT 1 FROM chunk_topic_assignments cta WHERE cta.chunk_id = c.id
                 )
                 AND p.name = $1 AND f.language = $2
                 ORDER BY f.path, c.chunk_index
                 LIMIT $3",
            )
            .bind(proj)
            .bind(lang)
            .bind(limit)
            .fetch_all(pool)
            .await
        }
        (Some(proj), None) => {
            sqlx::query_as::<_, OrphanChunkRow>(
                "SELECT c.id as chunk_id, c.content, f.path, f.language,
                        p.name as project_name, c.chunk_index
                 FROM file_chunks c
                 JOIN indexed_files f ON f.id = c.file_id
                 JOIN projects p ON p.id = f.project_id
                 WHERE NOT EXISTS (
                     SELECT 1 FROM chunk_topic_assignments cta WHERE cta.chunk_id = c.id
                 )
                 AND p.name = $1
                 ORDER BY f.path, c.chunk_index
                 LIMIT $2",
            )
            .bind(proj)
            .bind(limit)
            .fetch_all(pool)
            .await
        }
        (None, Some(lang)) => {
            sqlx::query_as::<_, OrphanChunkRow>(
                "SELECT c.id as chunk_id, c.content, f.path, f.language,
                        p.name as project_name, c.chunk_index
                 FROM file_chunks c
                 JOIN indexed_files f ON f.id = c.file_id
                 JOIN projects p ON p.id = f.project_id
                 WHERE NOT EXISTS (
                     SELECT 1 FROM chunk_topic_assignments cta WHERE cta.chunk_id = c.id
                 )
                 AND f.language = $1
                 ORDER BY f.path, c.chunk_index
                 LIMIT $2",
            )
            .bind(lang)
            .bind(limit)
            .fetch_all(pool)
            .await
        }
        (None, None) => {
            sqlx::query_as::<_, OrphanChunkRow>(
                "SELECT c.id as chunk_id, c.content, f.path, f.language,
                        p.name as project_name, c.chunk_index
                 FROM file_chunks c
                 JOIN indexed_files f ON f.id = c.file_id
                 JOIN projects p ON p.id = f.project_id
                 WHERE NOT EXISTS (
                     SELECT 1 FROM chunk_topic_assignments cta WHERE cta.chunk_id = c.id
                 )
                 ORDER BY f.path, c.chunk_index
                 LIMIT $1",
            )
            .bind(limit)
            .fetch_all(pool)
            .await
        }
    }
}

/// File-level orphan summary.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct OrphanFileSummary {
    pub path: String,
    pub project_name: String,
    pub language: String,
    pub orphan_chunks: i64,
    pub total_chunks: i64,
    pub orphan_pct: f64,
}

/// Get file-level summary of orphan chunks (files with highest orphan %).
pub async fn find_orphan_file_summary(
    pool: &PgPool,
    project: Option<&str>,
) -> Result<Vec<OrphanFileSummary>, sqlx::Error> {
    if let Some(proj) = project {
        sqlx::query_as::<_, OrphanFileSummary>(
            "SELECT f.path, p.name as project_name, f.language,
                    COUNT(*) FILTER (WHERE cta.chunk_id IS NULL) as orphan_chunks,
                    COUNT(*) as total_chunks,
                    ROUND(100.0 * COUNT(*) FILTER (WHERE cta.chunk_id IS NULL) / COUNT(*), 1)::float8 as orphan_pct
             FROM file_chunks c
             JOIN indexed_files f ON f.id = c.file_id
             JOIN projects p ON p.id = f.project_id
             LEFT JOIN chunk_topic_assignments cta ON cta.chunk_id = c.id
             WHERE p.name = $1
             GROUP BY f.id, f.path, p.name, f.language
             HAVING COUNT(*) FILTER (WHERE cta.chunk_id IS NULL) > 0
             ORDER BY orphan_pct DESC, orphan_chunks DESC"
        )
        .bind(proj)
        .fetch_all(pool)
        .await
    } else {
        sqlx::query_as::<_, OrphanFileSummary>(
            "SELECT f.path, p.name as project_name, f.language,
                    COUNT(*) FILTER (WHERE cta.chunk_id IS NULL) as orphan_chunks,
                    COUNT(*) as total_chunks,
                    ROUND(100.0 * COUNT(*) FILTER (WHERE cta.chunk_id IS NULL) / COUNT(*), 1)::float8 as orphan_pct
             FROM file_chunks c
             JOIN indexed_files f ON f.id = c.file_id
             JOIN projects p ON p.id = f.project_id
             LEFT JOIN chunk_topic_assignments cta ON cta.chunk_id = c.id
             GROUP BY f.id, f.path, p.name, f.language
             HAVING COUNT(*) FILTER (WHERE cta.chunk_id IS NULL) > 0
             ORDER BY orphan_pct DESC, orphan_chunks DESC"
        )
        .fetch_all(pool)
        .await
    }
}

/// File-to-topic assignment for misplaced code analysis.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct FileTopicRow {
    pub path: String,
    pub project_name: String,
    pub topic_label: String,
    pub topic_id: i32,
    pub chunks_in_topic: i64,
}

/// Load chunk-to-topic assignments aggregated to file level.
pub async fn load_chunk_topic_assignments_for_files(
    pool: &PgPool,
    project: Option<&str>,
) -> Result<Vec<FileTopicRow>, sqlx::Error> {
    // The five-way join + GROUP BY can scan the full chunk_topic_assignments
    // table; raise the per-transaction ceiling so the daemon-wide
    // statement_timeout doesn't fire mid-aggregation on large projects.
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout = '2min'")
        .execute(&mut *tx)
        .await?;
    let results = if let Some(proj) = project {
        sqlx::query_as::<_, FileTopicRow>(
            "SELECT f.path, p.name as project_name, ct.label as topic_label,
                    ct.id as topic_id, COUNT(*) as chunks_in_topic
             FROM chunk_topic_assignments cta
             JOIN file_chunks c ON c.id = cta.chunk_id
             JOIN indexed_files f ON f.id = c.file_id
             JOIN projects p ON p.id = f.project_id
             JOIN code_topics ct ON ct.id = cta.topic_id
             WHERE p.name = $1
             GROUP BY f.path, p.name, ct.label, ct.id
             ORDER BY f.path, chunks_in_topic DESC",
        )
        .bind(proj)
        .fetch_all(&mut *tx)
        .await?
    } else {
        sqlx::query_as::<_, FileTopicRow>(
            "SELECT f.path, p.name as project_name, ct.label as topic_label,
                    ct.id as topic_id, COUNT(*) as chunks_in_topic
             FROM chunk_topic_assignments cta
             JOIN file_chunks c ON c.id = cta.chunk_id
             JOIN indexed_files f ON f.id = c.file_id
             JOIN projects p ON p.id = f.project_id
             JOIN code_topics ct ON ct.id = cta.topic_id
             GROUP BY f.path, p.name, ct.label, ct.id
             ORDER BY f.path, chunks_in_topic DESC",
        )
        .fetch_all(&mut *tx)
        .await?
    };
    tx.commit().await?;
    Ok(results)
}

/// Co-change coupled file pair from git history.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct CoupledFilePair {
    pub file_a: String,
    pub file_b: String,
    pub co_commits: i64,
    pub commits_a: i64,
    pub commits_b: i64,
    pub jaccard: f64,
}

/// Find files that frequently change together in git commits (Jaccard co-change coupling).
pub async fn find_coupled_files(
    pool: &PgPool,
    project: &str,
    min_coupling: f64,
    min_commits: i32,
) -> Result<Vec<CoupledFilePair>, sqlx::Error> {
    sqlx::query_as::<_, CoupledFilePair>(
        "WITH file_commits AS (
            SELECT gcf.file_path, gcf.commit_id
            FROM git_commit_files gcf
            JOIN git_commits gc ON gc.id = gcf.commit_id
            JOIN projects p ON p.id = gc.project_id
            WHERE p.name = $1
        ),
        pair_counts AS (
            SELECT a.file_path AS file_a, b.file_path AS file_b,
                   COUNT(*) AS co_commits
            FROM file_commits a
            JOIN file_commits b ON a.commit_id = b.commit_id AND a.file_path < b.file_path
            GROUP BY a.file_path, b.file_path
        ),
        file_totals AS (
            SELECT file_path, COUNT(DISTINCT commit_id) AS total_commits
            FROM file_commits
            GROUP BY file_path
        )
        SELECT pc.file_a, pc.file_b, pc.co_commits,
               ta.total_commits AS commits_a, tb.total_commits AS commits_b,
               pc.co_commits::float8 / (ta.total_commits + tb.total_commits - pc.co_commits) AS jaccard
        FROM pair_counts pc
        JOIN file_totals ta ON ta.file_path = pc.file_a
        JOIN file_totals tb ON tb.file_path = pc.file_b
        WHERE pc.co_commits::float8 / (ta.total_commits + tb.total_commits - pc.co_commits) >= $2
          AND pc.co_commits >= $3
        ORDER BY jaccard DESC"
    )
    .bind(project)
    .bind(min_coupling)
    .bind(min_commits)
    .fetch_all(pool)
    .await
}

/// File complexity data for hotspot analysis.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct FileComplexityRow {
    pub path: String,
    pub language: String,
    pub size_bytes: i64,
    pub chunk_count: i64,
    pub topic_count: i64,
}

/// Get per-file complexity data (size, chunk count, topic diversity).
pub async fn get_file_complexity_data(
    pool: &PgPool,
    project: &str,
) -> Result<Vec<FileComplexityRow>, sqlx::Error> {
    sqlx::query_as::<_, FileComplexityRow>(
        "SELECT f.path, f.language, f.size_bytes,
                COUNT(DISTINCT c.id) as chunk_count,
                COUNT(DISTINCT cta.topic_id) as topic_count
         FROM indexed_files f
         JOIN projects p ON p.id = f.project_id
         JOIN file_chunks c ON c.file_id = f.id
         LEFT JOIN chunk_topic_assignments cta ON cta.chunk_id = c.id
         WHERE p.name = $1
         GROUP BY f.id, f.path, f.language, f.size_bytes
         ORDER BY chunk_count DESC",
    )
    .bind(project)
    .fetch_all(pool)
    .await
}

/// Topic coverage row for test gap analysis.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct TopicCoverageRow {
    pub topic_id: i32,
    pub label: String,
    pub test_chunks: i64,
    pub impl_chunks: i64,
}

/// Get per-topic test vs implementation chunk counts for a project.
pub async fn get_test_topic_coverage(
    pool: &PgPool,
    project: &str,
) -> Result<Vec<TopicCoverageRow>, sqlx::Error> {
    sqlx::query_as::<_, TopicCoverageRow>(
        "SELECT ct.id as topic_id, ct.label,
                COUNT(*) FILTER (WHERE f.path ~ '(^|/)(tests?|specs?)(/|$)|_test\\.|\\btest_|\\.test\\.|_spec\\.|\\bspec_|\\.spec\\.') as test_chunks,
                COUNT(*) FILTER (WHERE f.path !~ '(^|/)(tests?|specs?)(/|$)|_test\\.|\\btest_|\\.test\\.|_spec\\.|\\bspec_|\\.spec\\.') as impl_chunks
         FROM chunk_topic_assignments cta
         JOIN file_chunks c ON c.id = cta.chunk_id
         JOIN indexed_files f ON f.id = c.file_id
         JOIN projects p ON p.id = f.project_id
         JOIN code_topics ct ON ct.id = cta.topic_id
         WHERE p.name = $1
         GROUP BY ct.id, ct.label
         ORDER BY impl_chunks DESC"
    )
    .bind(project)
    .fetch_all(pool)
    .await
}

/// Topic centroid row for hierarchy analysis.
#[derive(Debug, Clone)]
pub struct TopicCentroidRow {
    pub topic_id: i32,
    pub label: String,
    pub chunk_count: i64,
    pub centroid: Vec<f32>,
}

/// Load topic centroids by averaging chunk embeddings per topic.
/// Since pgvector may not support AVG on vector, we compute centroids in Rust.
pub async fn load_topic_centroids(
    pool: &PgPool,
    scope: &str,
) -> Result<Vec<TopicCentroidRow>, sqlx::Error> {
    // First get the topic metadata
    let topics = sqlx::query_as::<_, TopicMetaRow>(
        "SELECT id as topic_id, label, chunk_count
         FROM code_topics
         WHERE scope = $1
         ORDER BY chunk_count DESC",
    )
    .bind(scope)
    .fetch_all(pool)
    .await?;

    let mut results = Vec::with_capacity(topics.len());

    for topic in &topics {
        // Get all chunk embeddings for this topic
        let embeddings: Vec<Vec<f32>> = sqlx::query_scalar::<_, Vec<f32>>(
            "SELECT c.embedding::real[] as embedding
             FROM chunk_topic_assignments cta
             JOIN file_chunks c ON c.id = cta.chunk_id
             WHERE cta.topic_id = $1",
        )
        .bind(topic.topic_id)
        .fetch_all(pool)
        .await?;

        if embeddings.is_empty() {
            continue;
        }

        // Compute centroid as mean of all embeddings
        let dim = embeddings[0].len();
        let mut centroid = vec![0.0f32; dim];
        for emb in &embeddings {
            for (i, val) in emb.iter().enumerate() {
                if i < dim {
                    centroid[i] += val;
                }
            }
        }
        let n = embeddings.len() as f32;
        for val in &mut centroid {
            *val /= n;
        }

        // L2-normalize
        let norm: f32 = centroid.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for val in &mut centroid {
                *val /= norm;
            }
        }

        results.push(TopicCentroidRow {
            topic_id: topic.topic_id,
            label: topic.label.clone(),
            chunk_count: topic.chunk_count as i64,
            centroid,
        });
    }

    Ok(results)
}

#[derive(Debug, sqlx::FromRow)]
struct TopicMetaRow {
    topic_id: i32,
    label: String,
    chunk_count: i32,
}

/// Check whether any chunk_topic_assignments exist (to detect if topics have been computed).
pub async fn has_topic_assignments(pool: &PgPool) -> Result<bool, sqlx::Error> {
    let count =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM chunk_topic_assignments LIMIT 1")
            .fetch_one(pool)
            .await?;
    Ok(count > 0)
}

/// Check if git_commit_files has data for a given project.
pub async fn has_commit_files_for_project(
    pool: &PgPool,
    project: &str,
) -> Result<bool, sqlx::Error> {
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM git_commit_files gcf
         JOIN git_commits gc ON gc.id = gcf.commit_id
         JOIN projects p ON p.id = gc.project_id
         WHERE p.name = $1
         LIMIT 1",
    )
    .bind(project)
    .fetch_one(pool)
    .await?;
    Ok(count > 0)
}

// ============================================================================
// Document analysis queries (suggest_merges, suggest_splits, doc_coverage_gaps)
// ============================================================================

/// Per-file topic distribution row for merge analysis.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct FileTopicDistributionRow {
    pub file_id: i64,
    pub path: String,
    pub relative_path: String,
    pub language: String,
    pub line_count: i32,
    pub size_bytes: i64,
    pub topic_id: i32,
    pub topic_label: String,
    pub keywords: Option<Vec<String>>,
    pub total_membership: f64,
    pub chunks_in_topic: i64,
}

/// Get per-file topic distributions for merge analysis.
/// Returns one row per (file, topic) pair with aggregated membership scores.
pub async fn get_file_topic_distributions(
    pool: &PgPool,
    project: &str,
    language: Option<&str>,
) -> Result<Vec<FileTopicDistributionRow>, sqlx::Error> {
    sqlx::query_as::<_, FileTopicDistributionRow>(
        "SELECT f.id as file_id, f.path, f.relative_path, f.language,
                f.line_count, f.size_bytes,
                cta.topic_id, ct.label as topic_label, ct.keywords,
                SUM(cta.membership_score) as total_membership,
                COUNT(*) as chunks_in_topic
         FROM indexed_files f
         JOIN projects p ON p.id = f.project_id
         JOIN file_chunks c ON c.file_id = f.id
         JOIN chunk_topic_assignments cta ON cta.chunk_id = c.id
         JOIN code_topics ct ON ct.id = cta.topic_id
         WHERE p.name = $1
           AND ($2::text IS NULL OR f.language = $2)
         GROUP BY f.id, f.path, f.relative_path, f.language,
                  f.line_count, f.size_bytes,
                  cta.topic_id, ct.label, ct.keywords
         ORDER BY f.path, total_membership DESC",
    )
    .bind(project)
    .bind(language)
    .fetch_all(pool)
    .await
}

/// Chunk-level topic detail row for split analysis.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct ChunkTopicDetailRow {
    pub file_id: i64,
    pub path: String,
    pub relative_path: String,
    pub language: String,
    pub line_count: i32,
    pub size_bytes: i64,
    pub chunk_id: i64,
    pub chunk_index: i32,
    pub start_line: i32,
    pub end_line: i32,
    pub chunk_content: String,
    pub topic_id: i32,
    pub topic_label: String,
    pub membership_score: f64,
}

/// Get chunk-level topic assignments with position info for split analysis.
pub async fn get_chunk_topic_details(
    pool: &PgPool,
    project: &str,
    language: Option<&str>,
) -> Result<Vec<ChunkTopicDetailRow>, sqlx::Error> {
    sqlx::query_as::<_, ChunkTopicDetailRow>(
        "SELECT f.id as file_id, f.path, f.relative_path, f.language,
                f.line_count, f.size_bytes,
                c.id as chunk_id, c.chunk_index, c.start_line, c.end_line,
                c.content as chunk_content,
                cta.topic_id, ct.label as topic_label,
                cta.membership_score
         FROM indexed_files f
         JOIN projects p ON p.id = f.project_id
         JOIN file_chunks c ON c.file_id = f.id
         JOIN chunk_topic_assignments cta ON cta.chunk_id = c.id
         JOIN code_topics ct ON ct.id = cta.topic_id
         WHERE p.name = $1
           AND ($2::text IS NULL OR f.language = $2)
         ORDER BY f.path, c.chunk_index, cta.membership_score DESC",
    )
    .bind(project)
    .bind(language)
    .fetch_all(pool)
    .await
}

/// Documentation coverage row for doc_coverage_gaps analysis.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct DocCoverageRow {
    pub topic_id: i32,
    pub label: String,
    pub keywords: Option<Vec<String>>,
    pub doc_chunks: i64,
    pub code_chunks: i64,
}

/// Get per-topic documentation vs code chunk counts for a project.
pub async fn get_doc_topic_coverage(
    pool: &PgPool,
    project: &str,
) -> Result<Vec<DocCoverageRow>, sqlx::Error> {
    sqlx::query_as::<_, DocCoverageRow>(
        "SELECT ct.id as topic_id, ct.label, ct.keywords,
                COUNT(*) FILTER (WHERE f.language = 'markdown') as doc_chunks,
                COUNT(*) FILTER (WHERE f.language != 'markdown') as code_chunks
         FROM chunk_topic_assignments cta
         JOIN file_chunks c ON c.id = cta.chunk_id
         JOIN indexed_files f ON f.id = c.file_id
         JOIN projects p ON p.id = f.project_id
         JOIN code_topics ct ON ct.id = cta.topic_id
         WHERE p.name = $1
         GROUP BY ct.id, ct.label, ct.keywords
         ORDER BY code_chunks DESC",
    )
    .bind(project)
    .fetch_all(pool)
    .await
}

/// Clean up stale files (files that no longer exist on disk).
pub async fn cleanup_stale_files(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let paths = sqlx::query_scalar::<_, String>("SELECT path FROM indexed_files")
        .fetch_all(pool)
        .await?;

    let mut removed = 0u64;
    for path in &paths {
        if !std::path::Path::new(path).exists() {
            sqlx::query("DELETE FROM indexed_files WHERE path = $1")
                .bind(path)
                .execute(pool)
                .await?;
            removed += 1;
        }
    }

    Ok(removed)
}

// ============================================================================
// Status snapshot — consumed by `pgmcp status` CLI and `/api/status` REST
// ============================================================================

/// Per-table freshness rollup for the `pgmcp status` output. Every field
/// is derived from a single batched SQL round-trip in
/// [`status_snapshot`]. Counts are cheap (`COUNT(*)`) — for tables with
/// millions of rows they're still served by PG's planner against
/// pre-computed pg_class.reltuples in practice. The exact counts here
/// are observability data, not authoritative.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StatusSnapshot {
    pub project_count: i64,
    pub indexed_file_count: i64,
    pub chunk_count: i64,
    pub git_commit_count: i64,
    pub git_commit_chunk_count: i64,

    pub topic_count_global: i64,
    pub topic_count_total: i64,
    pub topic_assignments_total: i64,
    pub topic_last_computed: Option<DateTime<Utc>>,
    pub topic_noise_chunk_count: i64,
    pub topic_breakdown_by_scope: Vec<TopicScopeStat>,

    pub similarity_pair_count: i64,
    pub similarity_distinct_files: i64,
    pub similarity_last_computed: Option<DateTime<Utc>>,

    pub file_metric_count: i64,
    pub graph_edge_count: i64,
    pub graph_edges_by_type: Vec<EdgeTypeCount>,
    pub graph_metric_last_computed: Option<DateTime<Utc>>,
    pub graph_edge_last_computed: Option<DateTime<Utc>>,

    pub blame_coverage_with: i64,
    pub blame_coverage_total: i64,

    pub per_project: Vec<PerProjectStat>,
    pub git_per_project: Vec<GitProjectStat>,

    pub last_indexed_at: Option<DateTime<Utc>>,
    pub server_version: Option<String>,
    pub vector_extension_version: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct EdgeTypeCount {
    pub edge_type: String,
    pub count: i64,
}

/// Per-(scope, topic) summary used by `pgmcp status topics`. `scope`
/// is `'*'` for the global cron-driven scan; per-project scopes look
/// like `'project:<name>'`. `last_computed` is the MAX(`computed_at`)
/// across all topics in that scope.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TopicScopeStat {
    pub scope: String,
    pub topic_count: i64,
    pub last_computed: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PerProjectStat {
    pub project_name: String,
    pub indexed_file_count: i64,
    pub chunk_count: i64,
    pub file_metric_count: i64,
    pub last_indexed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GitProjectStat {
    pub project_name: String,
    pub commit_count: i64,
    pub last_commit_hash: Option<String>,
    pub last_commit_date: Option<DateTime<Utc>>,
}

/// Tuple shape of the per-project rollup query.
type PerProjectRow = (String, i64, i64, i64, Option<DateTime<Utc>>);
/// Tuple shape of the per-project git rollup query.
type GitProjectRow = (String, i64, Option<String>, Option<DateTime<Utc>>);

/// Read every counter + timestamp the status command needs, in one
/// transaction. Each query is `COUNT(*)` or `MAX(timestamp)` — cheap
/// enough that a single status call is fine to issue against a busy
/// daemon.
pub async fn status_snapshot(pool: &PgPool) -> Result<StatusSnapshot, sqlx::Error> {
    // Counts (every table has its own access pattern; do them in one
    // sequential transaction so a single connection serves the whole
    // request — no pool churn).
    let mut tx = pool.begin().await?;

    let project_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM projects")
        .fetch_one(&mut *tx)
        .await?;
    let indexed_file_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM indexed_files")
        .fetch_one(&mut *tx)
        .await?;
    let chunk_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM file_chunks")
        .fetch_one(&mut *tx)
        .await?;

    // git tables only exist if the migrations created them — but they
    // always do in pgmcp, so unconditional COUNT is safe.
    let git_commit_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM git_commits")
        .fetch_one(&mut *tx)
        .await
        .unwrap_or(0);
    let git_commit_chunk_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM git_commit_chunks")
        .fetch_one(&mut *tx)
        .await
        .unwrap_or(0);

    // `scope = 'global'` is what `cron::topic_clustering::run_global_topic_scan`
    // writes (NOT `'*'` — that string is the MCP tool API "match-all"
    // *parameter*, not a stored scope value).
    let topic_count_global: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM code_topics WHERE scope = 'global'")
            .fetch_one(&mut *tx)
            .await
            .unwrap_or(0);
    let topic_count_total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM code_topics")
        .fetch_one(&mut *tx)
        .await
        .unwrap_or(0);
    let topic_assignments_total: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM chunk_topic_assignments")
            .fetch_one(&mut *tx)
            .await
            .unwrap_or(0);
    let topic_last_computed: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT MAX(computed_at) FROM code_topics")
            .fetch_one(&mut *tx)
            .await
            .unwrap_or(None);

    // Noise = chunks with NO entry in chunk_topic_assignments. Only
    // meaningful once topics have been computed (otherwise everything
    // is "noise" trivially); the CLI labels the field accordingly.
    let topic_noise_chunk_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM file_chunks c \
         WHERE NOT EXISTS (SELECT 1 FROM chunk_topic_assignments a WHERE a.chunk_id = c.id)",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap_or(0);

    // Per-scope topic breakdown (one row per distinct scope).
    let topic_scope_rows: Vec<(String, i64, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT scope, COUNT(*)::BIGINT, MAX(computed_at) \
         FROM code_topics GROUP BY scope ORDER BY scope",
    )
    .fetch_all(&mut *tx)
    .await
    .unwrap_or_default();
    let topic_breakdown_by_scope: Vec<TopicScopeStat> = topic_scope_rows
        .into_iter()
        .map(|(scope, topic_count, last_computed)| TopicScopeStat {
            scope,
            topic_count,
            last_computed,
        })
        .collect();

    let similarity_pair_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM cross_project_similarities")
            .fetch_one(&mut *tx)
            .await
            .unwrap_or(0);
    let similarity_distinct_files: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM (
             SELECT file_id_a AS f FROM cross_project_similarities
             UNION
             SELECT file_id_b AS f FROM cross_project_similarities
         ) AS u",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap_or(0);
    let similarity_last_computed: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT MAX(computed_at) FROM cross_project_similarities")
            .fetch_one(&mut *tx)
            .await
            .unwrap_or(None);

    let file_metric_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM file_metrics")
        .fetch_one(&mut *tx)
        .await
        .unwrap_or(0);
    let graph_edge_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM code_graph_edges")
        .fetch_one(&mut *tx)
        .await
        .unwrap_or(0);
    let graph_edge_rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT edge_type, COUNT(*)::BIGINT FROM code_graph_edges \
         GROUP BY edge_type ORDER BY edge_type",
    )
    .fetch_all(&mut *tx)
    .await
    .unwrap_or_default();
    let graph_edges_by_type: Vec<EdgeTypeCount> = graph_edge_rows
        .into_iter()
        .map(|(edge_type, count)| EdgeTypeCount { edge_type, count })
        .collect();
    let graph_metric_last_computed: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT MAX(computed_at) FROM file_metrics")
            .fetch_one(&mut *tx)
            .await
            .unwrap_or(None);
    let graph_edge_last_computed: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT MAX(computed_at) FROM code_graph_edges")
            .fetch_one(&mut *tx)
            .await
            .unwrap_or(None);

    // Blame coverage on file_chunks. blame_commit is added by an
    // ALTER in migrations and is NULL until the git-history-index
    // cron has populated it.
    let blame_coverage_with: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM file_chunks WHERE blame_commit IS NOT NULL")
            .fetch_one(&mut *tx)
            .await
            .unwrap_or(0);
    let blame_coverage_total: i64 = chunk_count;

    // Per-project breakdown: indexed_files, chunks, file_metrics,
    // last_indexed for every project. Single LEFT-JOIN'd query.
    let per_project_rows: Vec<PerProjectRow> = sqlx::query_as(
        "SELECT
             p.name,
             COUNT(DISTINCT f.id)::BIGINT,
             COUNT(c.id)::BIGINT,
             COUNT(DISTINCT fm.file_id)::BIGINT,
             MAX(f.modified_at)
         FROM projects p
         LEFT JOIN indexed_files f ON f.project_id = p.id
         LEFT JOIN file_chunks c ON c.file_id = f.id
         LEFT JOIN file_metrics fm ON fm.file_id = f.id
         GROUP BY p.id, p.name
         ORDER BY p.name",
    )
    .fetch_all(&mut *tx)
    .await
    .unwrap_or_default();
    let per_project: Vec<PerProjectStat> = per_project_rows
        .into_iter()
        .map(
            |(
                project_name,
                indexed_file_count,
                chunk_count,
                file_metric_count,
                last_indexed_at,
            )| {
                PerProjectStat {
                    project_name,
                    indexed_file_count,
                    chunk_count,
                    file_metric_count,
                    last_indexed_at,
                }
            },
        )
        .collect();

    // Per-project git breakdown: commit_count, last_commit (by date).
    let git_per_project_rows: Vec<GitProjectRow> = sqlx::query_as(
        "SELECT p.name,
                    COUNT(gc.id)::BIGINT,
                    (ARRAY_AGG(gc.commit_hash ORDER BY gc.author_date DESC))[1],
                    MAX(gc.author_date)
             FROM projects p
             LEFT JOIN git_commits gc ON gc.project_id = p.id
             GROUP BY p.id, p.name
             HAVING COUNT(gc.id) > 0
             ORDER BY p.name",
    )
    .fetch_all(&mut *tx)
    .await
    .unwrap_or_default();
    let git_per_project: Vec<GitProjectStat> = git_per_project_rows
        .into_iter()
        .map(
            |(project_name, commit_count, last_commit_hash, last_commit_date)| GitProjectStat {
                project_name,
                commit_count,
                last_commit_hash,
                last_commit_date,
            },
        )
        .collect();

    let last_indexed_at: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT MAX(modified_at) FROM indexed_files")
            .fetch_one(&mut *tx)
            .await
            .unwrap_or(None);

    let server_version: Option<String> = sqlx::query_scalar("SHOW server_version")
        .fetch_one(&mut *tx)
        .await
        .ok();
    let vector_extension_version: Option<String> =
        sqlx::query_scalar("SELECT extversion FROM pg_extension WHERE extname = 'vector'")
            .fetch_one(&mut *tx)
            .await
            .ok();

    tx.commit().await?;

    Ok(StatusSnapshot {
        project_count,
        indexed_file_count,
        chunk_count,
        git_commit_count,
        git_commit_chunk_count,
        topic_count_global,
        topic_count_total,
        topic_assignments_total,
        topic_last_computed,
        topic_noise_chunk_count,
        topic_breakdown_by_scope,
        similarity_pair_count,
        similarity_distinct_files,
        similarity_last_computed,
        file_metric_count,
        graph_edge_count,
        graph_edges_by_type,
        graph_metric_last_computed,
        graph_edge_last_computed,
        blame_coverage_with,
        blame_coverage_total,
        per_project,
        git_per_project,
        last_indexed_at,
        server_version,
        vector_extension_version,
    })
}

// ============================================================================
// Tier-0e — Symbol extraction (file_symbols + symbol_references)
// ============================================================================

/// One row backing the symbol-extraction Phase A scan.
#[derive(Debug, sqlx::FromRow)]
pub struct SymbolExtractionFileMeta {
    pub file_id: i64,
    pub relative_path: String,
    pub language: String,
}

/// Phase-A metadata fetch — per-project list of files routed to a backend that exists,
/// optionally filtered by `since` watermark.
pub async fn list_files_for_symbol_extraction(
    pool: &PgPool,
    project_id: i32,
    backend_languages: &[&str],
    since: Option<DateTime<Utc>>,
) -> Result<Vec<SymbolExtractionFileMeta>, sqlx::Error> {
    let langs: Vec<String> = backend_languages.iter().map(|s| s.to_string()).collect();
    sqlx::query_as::<_, SymbolExtractionFileMeta>(
        "SELECT id as file_id, relative_path, language
         FROM indexed_files
         WHERE project_id = $1
           AND content IS NOT NULL
           AND language = ANY($2::text[])
           AND ($3::timestamptz IS NULL OR modified_at > $3)
         ORDER BY id",
    )
    .bind(project_id)
    .bind(&langs)
    .bind(since)
    .fetch_all(pool)
    .await
}

/// Per-batch content fetch for the symbol-extraction cron's Phase B.
#[derive(Debug, sqlx::FromRow)]
pub struct SymbolExtractionFileContent {
    pub file_id: i64,
    pub relative_path: String,
    pub language: String,
    pub content: Option<String>,
}

/// Fetch content for a batch of file IDs.
pub async fn fetch_file_content_batch(
    pool: &PgPool,
    project_id: i32,
    file_ids: &[i64],
) -> Result<Vec<SymbolExtractionFileContent>, sqlx::Error> {
    sqlx::query_as::<_, SymbolExtractionFileContent>(
        "SELECT id as file_id, relative_path, language, content
         FROM indexed_files
         WHERE project_id = $1 AND id = ANY($2::bigint[]) AND content IS NOT NULL",
    )
    .bind(project_id)
    .bind(file_ids)
    .fetch_all(pool)
    .await
}

/// Delete all `file_symbols` rows for a file (CASCADE wipes children + dependent
/// `symbol_references` via the FK on `source_symbol_id`/`target_symbol_id`).
pub async fn delete_symbols_for_file(pool: &PgPool, file_id: i64) -> Result<u64, sqlx::Error> {
    let res = sqlx::query("DELETE FROM file_symbols WHERE file_id = $1")
        .bind(file_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Delete all `symbol_references` rows whose source is the given file.
pub async fn delete_symbol_references_for_file(
    pool: &PgPool,
    source_file_id: i64,
) -> Result<u64, sqlx::Error> {
    let res = sqlx::query("DELETE FROM symbol_references WHERE source_file_id = $1")
        .bind(source_file_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Bulk-insert symbols for a file via UNNEST. Caller must dedupe by
/// `(file_id, kind, name, start_line)` before invoking. Returns the inserted
/// row IDs **in input order**, so the cron can resolve `parent_id` (impl-method
/// → struct) by joining names within the same file.
///
/// On UNIQUE conflict (which should not happen if the caller deletes existing
/// rows first), `DO UPDATE` updates the metadata fields and returns the existing
/// id — preserving the input-order invariant.
pub async fn bulk_insert_file_symbols(
    pool: &PgPool,
    file_id: i64,
    symbols: &[crate::parsing::symbols::Symbol],
) -> Result<Vec<i64>, sqlx::Error> {
    if symbols.is_empty() {
        return Ok(Vec::new());
    }

    let names: Vec<String> = symbols.iter().map(|s| s.name.clone()).collect();
    let kinds: Vec<String> = symbols
        .iter()
        .map(|s| s.kind.as_db_str().to_string())
        .collect();
    let start_lines: Vec<i32> = symbols.iter().map(|s| s.start_line as i32).collect();
    let end_lines: Vec<i32> = symbols.iter().map(|s| s.end_line as i32).collect();
    let visibilities: Vec<Option<String>> = symbols.iter().map(|s| s.visibility.clone()).collect();
    let signatures: Vec<Option<String>> = symbols.iter().map(|s| s.signature.clone()).collect();

    // Shadow-ASR fields. Defaulted to None / empty arrays so backends that
    // haven't been upgraded yet still produce well-typed inputs.
    let return_type_raws: Vec<Option<String>> = symbols
        .iter()
        .map(|s| s.return_type.as_ref().and_then(|rt| rt.type_raw.clone()))
        .collect();
    // Per-symbol return_type_tags as JSON arrays. Postgres `text[][]` would
    // require ragged-array support that sqlx doesn't ship, so we wrap each
    // symbol's tag list in a JSONB scalar and expand it server-side.
    let return_type_tags_json: Vec<serde_json::Value> = symbols
        .iter()
        .map(|s| {
            let tags = s
                .return_type
                .as_ref()
                .map(|rt| rt.type_tags.clone())
                .unwrap_or_default();
            serde_json::Value::Array(tags.into_iter().map(serde_json::Value::String).collect())
        })
        .collect();
    let return_type_shapes: Vec<Option<serde_json::Value>> = symbols
        .iter()
        .map(|s| {
            s.return_type
                .as_ref()
                .and_then(|rt| rt.type_shape.as_ref())
                .and_then(|sh| serde_json::to_value(sh).ok())
        })
        .collect();
    let generic_params: Vec<Option<serde_json::Value>> = symbols
        .iter()
        .map(|s| {
            if s.generic_params.is_empty() {
                None
            } else {
                serde_json::to_value(&s.generic_params).ok()
            }
        })
        .collect();
    let scope_paths: Vec<Option<String>> = symbols.iter().map(|s| s.scope_path.clone()).collect();
    let scope_depths: Vec<Option<i32>> = symbols
        .iter()
        .map(|s| s.scope_depth.map(|d| d as i32))
        .collect();

    // Generate a per-batch ordinal so RETURNING comes back in input order
    // even when ON CONFLICT DO UPDATE fires.
    let ordinals: Vec<i32> = (0..symbols.len() as i32).collect();

    let rows: Vec<(i32, i64)> = sqlx::query_as::<_, (i32, i64)>(
        "WITH input AS (
             SELECT u.*,
                    COALESCE(
                        ARRAY(SELECT jsonb_array_elements_text(u.return_type_tags_json)),
                        '{}'::text[]
                    ) AS return_type_tags
             FROM UNNEST(
                 $1::int4[], $2::int8[], $3::text[], $4::text[],
                 $5::int4[], $6::int4[], $7::text[], $8::text[],
                 $9::text[], $10::jsonb[], $11::jsonb[], $12::jsonb[],
                 $13::text[], $14::int4[]
             ) AS u(
                 ord, file_id, name, kind, start_line, end_line, visibility, signature,
                 return_type_raw, return_type_tags_json, return_type_shape, generic_params,
                 scope_path, scope_depth
             )
         ),
         inserted AS (
             INSERT INTO file_symbols (
                 file_id, name, kind, start_line, end_line, visibility, signature,
                 return_type_raw, return_type_tags, return_type_shape, generic_params,
                 scope_path, scope_depth
             )
             SELECT file_id, name, kind, start_line, end_line, visibility, signature,
                    return_type_raw, return_type_tags, return_type_shape, generic_params,
                    scope_path, scope_depth
             FROM input
             ON CONFLICT (file_id, kind, name, start_line) DO UPDATE SET
                 end_line = EXCLUDED.end_line,
                 visibility = EXCLUDED.visibility,
                 signature = EXCLUDED.signature,
                 return_type_raw = EXCLUDED.return_type_raw,
                 return_type_tags = EXCLUDED.return_type_tags,
                 return_type_shape = EXCLUDED.return_type_shape,
                 generic_params = EXCLUDED.generic_params,
                 scope_path = EXCLUDED.scope_path,
                 scope_depth = EXCLUDED.scope_depth
             RETURNING id, file_id, kind, name, start_line
         )
         SELECT input.ord, inserted.id
         FROM input
         JOIN inserted USING (file_id, kind, name, start_line)
         ORDER BY input.ord",
    )
    .bind(&ordinals)
    .bind(vec![file_id; symbols.len()])
    .bind(&names)
    .bind(&kinds)
    .bind(&start_lines)
    .bind(&end_lines)
    .bind(&visibilities)
    .bind(&signatures)
    .bind(&return_type_raws)
    .bind(&return_type_tags_json)
    .bind(&return_type_shapes)
    .bind(&generic_params)
    .bind(&scope_paths)
    .bind(&scope_depths)
    .fetch_all(pool)
    .await?;

    let mut ids: Vec<i64> = vec![0i64; symbols.len()];
    for (ord, id) in rows {
        if let Some(slot) = ids.get_mut(ord as usize) {
            *slot = id;
        }
    }
    Ok(ids)
}

/// Bulk-insert the structured parameter rows that go with each symbol.
/// `symbol_ids` must align 1:1 with `symbols` (typically what
/// `bulk_insert_file_symbols` returned). Existing rows for a given
/// `symbol_id` are deleted first so re-runs replace the parameter set
/// rather than accumulating duplicates.
pub async fn bulk_insert_symbol_parameters(
    pool: &PgPool,
    symbol_ids: &[i64],
    symbols: &[crate::parsing::symbols::Symbol],
) -> Result<u64, sqlx::Error> {
    debug_assert_eq!(symbol_ids.len(), symbols.len());
    if symbols.is_empty() {
        return Ok(0);
    }

    // Flatten (symbol_id, parameter) pairs into column-oriented vecs.
    // type_tags is the only ragged column — encode as JSONB and expand
    // server-side via `jsonb_array_elements_text`.
    let mut sids: Vec<i64> = Vec::new();
    let mut positions: Vec<i32> = Vec::new();
    let mut names: Vec<Option<String>> = Vec::new();
    let mut type_raws: Vec<Option<String>> = Vec::new();
    let mut type_tags_json: Vec<serde_json::Value> = Vec::new();
    let mut type_shapes: Vec<Option<serde_json::Value>> = Vec::new();
    let mut default_values: Vec<Option<String>> = Vec::new();
    let mut modifiers: Vec<Option<String>> = Vec::new();
    let mut is_variadics: Vec<bool> = Vec::new();
    let mut is_selfs: Vec<bool> = Vec::new();
    let mut affected_sids: Vec<i64> = Vec::new();

    for (sid, sym) in symbol_ids.iter().zip(symbols.iter()) {
        if !sym.parameters.is_empty() {
            affected_sids.push(*sid);
        }
        for p in &sym.parameters {
            sids.push(*sid);
            positions.push(p.position as i32);
            names.push(p.name.clone());
            type_raws.push(p.type_raw.clone());
            type_tags_json.push(serde_json::Value::Array(
                p.type_tags
                    .iter()
                    .cloned()
                    .map(serde_json::Value::String)
                    .collect(),
            ));
            type_shapes.push(
                p.type_shape
                    .as_ref()
                    .and_then(|sh| serde_json::to_value(sh).ok()),
            );
            default_values.push(p.default_value.clone());
            modifiers.push(p.modifier.map(|m| m.as_db_str().to_string()));
            is_variadics.push(p.is_variadic);
            is_selfs.push(p.is_self);
        }
    }

    let mut tx = pool.begin().await?;

    // Replace semantics: clear out the existing parameters for the symbols
    // we're about to write, so a backend re-run that produces a different
    // signature shape doesn't leave orphan rows from the previous run.
    if !affected_sids.is_empty() {
        sqlx::query("DELETE FROM symbol_parameters WHERE symbol_id = ANY($1::int8[])")
            .bind(&affected_sids)
            .execute(&mut *tx)
            .await?;
    }

    if !sids.is_empty() {
        sqlx::query(
            "INSERT INTO symbol_parameters (
                 symbol_id, position, name, type_raw, type_tags, type_shape,
                 default_value, modifier, is_variadic, is_self
             )
             SELECT
                 symbol_id, position, name, type_raw,
                 COALESCE(
                     ARRAY(SELECT jsonb_array_elements_text(type_tags_json)),
                     '{}'::text[]
                 ) AS type_tags,
                 type_shape,
                 default_value, modifier, is_variadic, is_self
             FROM UNNEST(
                 $1::int8[], $2::int4[], $3::text[], $4::text[],
                 $5::jsonb[], $6::jsonb[],
                 $7::text[], $8::text[], $9::bool[], $10::bool[]
             ) AS u(
                 symbol_id, position, name, type_raw,
                 type_tags_json, type_shape,
                 default_value, modifier, is_variadic, is_self
             )",
        )
        .bind(&sids)
        .bind(&positions)
        .bind(&names)
        .bind(&type_raws)
        .bind(&type_tags_json)
        .bind(&type_shapes)
        .bind(&default_values)
        .bind(&modifiers)
        .bind(&is_variadics)
        .bind(&is_selfs)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(sids.len() as u64)
}

/// Bulk-insert the effect membership rows for each symbol. Replace
/// semantics, same as `bulk_insert_symbol_parameters`: existing rows for
/// each `symbol_id` are deleted before insert. The effect names must
/// exist in `effect_catalog` (enforced by the FK).
pub async fn bulk_insert_symbol_effects(
    pool: &PgPool,
    symbol_ids: &[i64],
    symbols: &[crate::parsing::symbols::Symbol],
) -> Result<u64, sqlx::Error> {
    debug_assert_eq!(symbol_ids.len(), symbols.len());
    if symbols.is_empty() {
        return Ok(0);
    }

    let mut sids: Vec<i64> = Vec::new();
    let mut effects: Vec<String> = Vec::new();
    let mut affected_sids: Vec<i64> = Vec::new();

    for (sid, sym) in symbol_ids.iter().zip(symbols.iter()) {
        if !sym.effects.is_empty() {
            affected_sids.push(*sid);
        }
        for eff in &sym.effects {
            sids.push(*sid);
            effects.push(eff.clone());
        }
    }

    let mut tx = pool.begin().await?;

    if !affected_sids.is_empty() {
        sqlx::query("DELETE FROM symbol_effects WHERE symbol_id = ANY($1::int8[])")
            .bind(&affected_sids)
            .execute(&mut *tx)
            .await?;
    }

    if !sids.is_empty() {
        sqlx::query(
            "INSERT INTO symbol_effects (symbol_id, effect)
             SELECT * FROM UNNEST($1::int8[], $2::text[])
             ON CONFLICT (symbol_id, effect) DO NOTHING",
        )
        .bind(&sids)
        .bind(&effects)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(sids.len() as u64)
}

/// Apply resolution metadata to existing `symbol_references` rows. Pairs
/// align with rows by `(source_file_id, source_line, target_raw,
/// ref_kind)` — the same composite key the cron uses to identify them.
///
/// Each entry is `(source_file_id, source_line, target_raw, ref_kind,
/// target_path, resolution_kind, resolution_confidence)`. Rows that don't
/// match silently no-op (typical when reindex deleted them mid-run).
#[allow(clippy::type_complexity)]
pub async fn update_symbol_reference_resolutions(
    pool: &PgPool,
    rows: &[(i64, u32, String, String, Option<String>, String, f32)],
) -> Result<u64, sqlx::Error> {
    if rows.is_empty() {
        return Ok(0);
    }
    let source_files: Vec<i64> = rows.iter().map(|r| r.0).collect();
    let source_lines: Vec<i32> = rows.iter().map(|r| r.1 as i32).collect();
    let target_raws: Vec<String> = rows.iter().map(|r| r.2.clone()).collect();
    let ref_kinds: Vec<String> = rows.iter().map(|r| r.3.clone()).collect();
    let target_paths: Vec<Option<String>> = rows.iter().map(|r| r.4.clone()).collect();
    let resolution_kinds: Vec<String> = rows.iter().map(|r| r.5.clone()).collect();
    let confidences: Vec<f32> = rows.iter().map(|r| r.6).collect();
    let res = sqlx::query(
        "UPDATE symbol_references sr
         SET target_path = u.target_path,
             resolution_kind = u.resolution_kind,
             resolution_confidence = u.resolution_confidence
         FROM UNNEST(
             $1::int8[], $2::int4[], $3::text[], $4::text[],
             $5::text[], $6::text[], $7::real[]
         ) AS u(source_file_id, source_line, target_raw, ref_kind,
                target_path, resolution_kind, resolution_confidence)
         WHERE sr.source_file_id = u.source_file_id
           AND sr.source_line = u.source_line
           AND sr.target_raw = u.target_raw
           AND sr.ref_kind = u.ref_kind",
    )
    .bind(&source_files)
    .bind(&source_lines)
    .bind(&target_raws)
    .bind(&ref_kinds)
    .bind(&target_paths)
    .bind(&resolution_kinds)
    .bind(&confidences)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Apply resolved `parent_id` values for a file's symbols. The cron computes
/// `parent_id` by name+line-range matching in Rust; this helper writes them
/// back in one round-trip.
pub async fn update_symbol_parent_ids(
    pool: &PgPool,
    pairs: &[(i64, i64)], // (child_id, parent_id)
) -> Result<u64, sqlx::Error> {
    if pairs.is_empty() {
        return Ok(0);
    }
    let child_ids: Vec<i64> = pairs.iter().map(|(c, _)| *c).collect();
    let parent_ids: Vec<i64> = pairs.iter().map(|(_, p)| *p).collect();
    let res = sqlx::query(
        "UPDATE file_symbols
         SET parent_id = u.parent_id
         FROM UNNEST($1::int8[], $2::int8[]) AS u(child_id, parent_id)
         WHERE file_symbols.id = u.child_id",
    )
    .bind(&child_ids)
    .bind(&parent_ids)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Bulk-insert symbol references for a file via UNNEST. Caller must dedupe by
/// `(source_line, target_raw, ref_kind)` before invoking. ON CONFLICT DO NOTHING
/// — duplicate rows from re-runs are silently dropped.
pub async fn bulk_insert_symbol_references(
    pool: &PgPool,
    source_file_id: i64,
    refs: &[crate::parsing::symbols::SymbolReference],
) -> Result<u64, sqlx::Error> {
    if refs.is_empty() {
        return Ok(0);
    }

    let source_files: Vec<i64> = vec![source_file_id; refs.len()];
    let source_symbols: Vec<Option<i64>> = refs.iter().map(|r| r.source_symbol_id).collect();
    let target_files: Vec<Option<i64>> = refs.iter().map(|r| r.target_file_id).collect();
    let target_symbols: Vec<Option<i64>> = refs.iter().map(|r| r.target_symbol_id).collect();
    let target_raws: Vec<String> = refs.iter().map(|r| r.target_raw.clone()).collect();
    let ref_kinds: Vec<String> = refs
        .iter()
        .map(|r| r.ref_kind.as_db_str().to_string())
        .collect();
    let source_lines: Vec<i32> = refs.iter().map(|r| r.source_line as i32).collect();

    let res = sqlx::query(
        "INSERT INTO symbol_references (
             source_file_id, source_symbol_id, target_file_id, target_symbol_id,
             target_raw, ref_kind, source_line
         )
         SELECT * FROM UNNEST(
             $1::int8[], $2::int8[], $3::int8[], $4::int8[],
             $5::text[], $6::text[], $7::int4[]
         )
         ON CONFLICT (source_file_id, source_line, target_raw, ref_kind) DO NOTHING",
    )
    .bind(&source_files)
    .bind(&source_symbols)
    .bind(&target_files)
    .bind(&target_symbols)
    .bind(&target_raws)
    .bind(&ref_kinds)
    .bind(&source_lines)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Per-project second pass — resolve `target_symbol_id` and `target_file_id`
/// for any unresolved `symbol_references` rows by joining `target_raw` against
/// `file_symbols.name` within the project. Multi-match by name picks one
/// arbitrarily; the confidence score in downstream tools accounts for this.
pub async fn resolve_symbol_reference_targets(
    pool: &PgPool,
    project_id: i32,
) -> Result<u64, sqlx::Error> {
    // Resolution pass v2: three-phase walk that populates not only
    // `target_symbol_id` (legacy) but also `target_path`,
    // `resolution_kind`, and `resolution_confidence`. The phases are
    // ordered by precision so each phase only touches rows the earlier
    // ones couldn't resolve.
    //
    //   1. exact_in_file        — name matches a symbol in the same file
    //                            (confidence 1.0).
    //   2. exact_via_import     — name matches a symbol whose `scope_path`
    //                            corresponds to an import's `target_raw`
    //                            within the same project (confidence 0.95).
    //   3. bare_name_in_project — name matches some symbol elsewhere in the
    //                            project (confidence 0.5).
    //   4. unresolved           — final mark for everything else
    //                            (confidence 0.0, target_symbol_id NULL).
    //
    // Each UPDATE is gated by `resolution_kind IS NULL` so the earlier-tier
    // assignments stick even when a later phase would also match.

    // Phase 1: exact_in_file. Same source file, same name.
    let phase1 = sqlx::query(
        "UPDATE symbol_references sr
         SET target_symbol_id = fs.id,
             target_file_id = fs.file_id,
             target_path = fs.scope_path,
             resolution_kind = 'exact_in_file',
             resolution_confidence = 1.0
         FROM file_symbols fs
         WHERE fs.file_id = sr.source_file_id
           AND sr.target_raw = fs.name
           AND sr.resolution_kind IS NULL
           AND EXISTS (
               SELECT 1 FROM indexed_files f
                WHERE f.id = sr.source_file_id AND f.project_id = $1
           )",
    )
    .bind(project_id)
    .execute(pool)
    .await?;

    // Phase 2: exact_via_import. The reference's source file imports a
    // module/symbol whose `target_raw` ends with `::<name>` (or `.<name>`
    // for languages using dot-notation). Match against `scope_path` so the
    // resolution is namespace-aware.
    //
    // The UPDATE target alias `sr` is in scope ONLY for SET/WHERE/RETURNING
    // — Postgres rejects references to `sr` inside `JOIN ... ON` predicates
    // between FROM-list members with `invalid reference to FROM-clause
    // entry for table "sr"`. The `e.source_file_id = sr.source_file_id`
    // correlation belongs in WHERE, not in the JOIN ON. See plan
    // ~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md F2.
    let phase2 = sqlx::query(
        "UPDATE symbol_references sr
         SET target_symbol_id = fs.id,
             target_file_id = fs.file_id,
             target_path = fs.scope_path,
             resolution_kind = 'exact_via_import',
             resolution_confidence = 0.95
         FROM file_symbols fs
         JOIN indexed_files tgt_f ON tgt_f.id = fs.file_id
         JOIN code_graph_edges e
           ON e.target_file_id = fs.file_id
          AND e.edge_type = 'import'
         WHERE sr.target_raw = fs.name
           AND sr.resolution_kind IS NULL
           AND tgt_f.project_id = $1
           AND e.source_file_id = sr.source_file_id
           AND EXISTS (
               SELECT 1 FROM indexed_files f
                WHERE f.id = sr.source_file_id AND f.project_id = $1
           )",
    )
    .bind(project_id)
    .execute(pool)
    .await?;

    // Phase 3: bare_name_in_project. Match name within the project (legacy
    // behavior, kept for parity). When multiple matches exist, the database
    // picks one deterministically; downstream tools that need precision can
    // filter on `resolution_confidence >= 0.95`.
    //
    // `src_f` exists only to enforce the source file's `project_id`, but
    // its `JOIN ON src_f.id = sr.source_file_id` predicate references the
    // UPDATE target alias `sr`, which Postgres rejects (`invalid reference
    // to FROM-clause entry for table "sr"`). The cleanest fix is to lift
    // the source-file project filter into an EXISTS subquery mirroring
    // Phase 1's pattern — `src_f` is no longer in the FROM list at all.
    let phase3 = sqlx::query(
        "UPDATE symbol_references sr
         SET target_symbol_id = fs.id,
             target_file_id = fs.file_id,
             target_path = fs.scope_path,
             resolution_kind = 'bare_name_in_project',
             resolution_confidence = 0.5
         FROM file_symbols fs
         JOIN indexed_files tgt_f ON tgt_f.id = fs.file_id
         WHERE tgt_f.project_id = $1
           AND sr.target_raw = fs.name
           AND sr.resolution_kind IS NULL
           AND EXISTS (
               SELECT 1 FROM indexed_files src_f
                WHERE src_f.id = sr.source_file_id
                  AND src_f.project_id = $1
           )",
    )
    .bind(project_id)
    .execute(pool)
    .await?;

    // Phase 4: anything still unresolved within the project's references is
    // marked `unresolved` so tools can distinguish "we tried" from "not yet
    // processed".
    let phase4 = sqlx::query(
        "UPDATE symbol_references sr
         SET resolution_kind = 'unresolved',
             resolution_confidence = 0.0
         FROM indexed_files f
         WHERE sr.source_file_id = f.id
           AND f.project_id = $1
           AND sr.resolution_kind IS NULL",
    )
    .bind(project_id)
    .execute(pool)
    .await?;

    Ok(phase1.rows_affected()
        + phase2.rows_affected()
        + phase3.rows_affected()
        + phase4.rows_affected())
}

/// Read the symbol-extraction watermark for a project.
pub async fn get_symbol_extraction_watermark(
    pool: &PgPool,
    project_id: i32,
) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
    let key = format!("symbol_extraction_last_run:{}", project_id);
    let val: Option<String> = sqlx::query_scalar("SELECT value FROM pgmcp_metadata WHERE key = $1")
        .bind(&key)
        .fetch_optional(pool)
        .await?;
    Ok(val.and_then(|s| {
        DateTime::parse_from_rfc3339(&s)
            .ok()
            .map(|dt| dt.with_timezone(&Utc))
    }))
}

/// Set the symbol-extraction watermark for a project.
pub async fn set_symbol_extraction_watermark(
    pool: &PgPool,
    project_id: i32,
    ts: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    let key = format!("symbol_extraction_last_run:{}", project_id);
    sqlx::query(
        "INSERT INTO pgmcp_metadata (key, value) VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(&key)
    .bind(ts.to_rfc3339())
    .execute(pool)
    .await?;
    Ok(())
}

/// One symbol-derived import edge for the graph-analysis migration.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ImportFromSymbols {
    pub source_file_id: i64,
    pub target_raw: String,
    pub target_file_id: Option<i64>,
    pub source_line: i32,
}

/// Fetch all `import_use` symbol-references for a project's files. Used by
/// `graph_analysis::analyze_project` to materialize import edges without
/// re-parsing file content (the symbol-extraction cron has already run).
pub async fn get_imports_from_symbols(
    pool: &PgPool,
    project_id: i32,
    file_ids: &[i64],
) -> Result<Vec<ImportFromSymbols>, sqlx::Error> {
    if file_ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as::<_, ImportFromSymbols>(
        "SELECT sr.source_file_id,
                sr.target_raw,
                sr.target_file_id,
                sr.source_line
         FROM symbol_references sr
         JOIN indexed_files f ON f.id = sr.source_file_id
         WHERE f.project_id = $1
           AND sr.source_file_id = ANY($2::bigint[])
           AND sr.ref_kind = 'import_use'",
    )
    .bind(project_id)
    .bind(file_ids)
    .fetch_all(pool)
    .await
}

/// Return the subset of `file_ids` that have at least one row in
/// `symbol_references`. Used by graph_analysis to decide which files take
/// the symbol-aware path vs the regex fallback.
pub async fn file_ids_with_symbol_refs(
    pool: &PgPool,
    project_id: i32,
    file_ids: &[i64],
) -> Result<std::collections::HashSet<i64>, sqlx::Error> {
    if file_ids.is_empty() {
        return Ok(std::collections::HashSet::new());
    }
    let rows: Vec<(i64,)> = sqlx::query_as::<_, (i64,)>(
        "SELECT DISTINCT sr.source_file_id
         FROM symbol_references sr
         JOIN indexed_files f ON f.id = sr.source_file_id
         WHERE f.project_id = $1
           AND sr.source_file_id = ANY($2::bigint[])",
    )
    .bind(project_id)
    .bind(file_ids)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// One row of the per-project naming distribution: a symbol's name + kind +
/// containing file path. Consumed by `tool_naming_consistency` for in-Rust
/// per-(directory, kind) convention dominance analysis.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct NamingDistributionRow {
    pub symbol_name: String,
    pub kind: String,
    pub file_id: i64,
    pub relative_path: String,
    pub start_line: i32,
    pub language: String,
}

/// Fetch all symbol names + kinds for a project (optionally filtered by language).
/// Sorted by `(relative_path, start_line)` so the consumer's directory-grouping
/// stays stable across runs.
pub async fn get_naming_distribution(
    pool: &PgPool,
    project_id: i32,
    language: Option<&str>,
) -> Result<Vec<NamingDistributionRow>, sqlx::Error> {
    sqlx::query_as::<_, NamingDistributionRow>(
        "SELECT fs.name as symbol_name,
                fs.kind,
                fs.file_id,
                f.relative_path,
                fs.start_line,
                f.language
         FROM file_symbols fs
         JOIN indexed_files f ON fs.file_id = f.id
         WHERE f.project_id = $1
           AND ($2::text IS NULL OR f.language = $2)
         ORDER BY f.relative_path, fs.start_line",
    )
    .bind(project_id)
    .bind(language)
    .fetch_all(pool)
    .await
}

// ============================================================================
// SOTA Phase 1 — function_metrics + call-graph queries
// ============================================================================

/// One row identifying a function symbol in a file. Returned by
/// `lookup_function_symbol_ids` so the function-metrics cron can map
/// (name, start_line) → file_symbols.id.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FunctionSymbolRow {
    pub symbol_id: i64,
    pub name: String,
    pub start_line: i32,
}

/// Look up `file_symbols.id` for every function in a file. Returned ordered
/// by `(name, start_line)` for deterministic matching by callers.
pub async fn lookup_function_symbol_ids(
    pool: &PgPool,
    file_id: i64,
) -> Result<Vec<FunctionSymbolRow>, sqlx::Error> {
    sqlx::query_as::<_, FunctionSymbolRow>(
        "SELECT id as symbol_id, name, start_line
         FROM file_symbols
         WHERE file_id = $1 AND kind = 'function'",
    )
    .bind(file_id)
    .fetch_all(pool)
    .await
}

/// One row consumed by `upsert_function_metrics_batch`. Mirrors the table
/// columns 1:1.
#[derive(Debug, Clone)]
pub struct FunctionMetricsRow {
    pub function_id: i64,
    pub file_id: i64,
    pub project_id: i32,
    pub cyclomatic: i32,
    pub cognitive: i32,
    pub halstead_n1: i32,
    pub halstead_n2: i32,
    pub halstead_big_n1: i32,
    pub halstead_big_n2: i32,
    pub halstead_volume: f64,
    pub halstead_difficulty: f64,
    pub halstead_effort: f64,
    pub halstead_bugs: f64,
    pub npath: i64,
    pub npath_overflow: bool,
    pub loc: i32,
    pub comment_lines: i32,
    pub maintainability_index: f64,
    pub panic_paths: i32,
    pub unsafe_blocks: i32,
}

/// UNNEST-style bulk upsert into `function_metrics`. ON CONFLICT (function_id)
/// DO UPDATE refreshes every column except `fan_in`/`fan_out` (those are owned
/// by the call-graph cron).
pub async fn upsert_function_metrics_batch(
    pool: &PgPool,
    rows: &[FunctionMetricsRow],
) -> Result<u64, sqlx::Error> {
    if rows.is_empty() {
        return Ok(0);
    }
    let function_ids: Vec<i64> = rows.iter().map(|r| r.function_id).collect();
    let file_ids: Vec<i64> = rows.iter().map(|r| r.file_id).collect();
    let project_ids: Vec<i32> = rows.iter().map(|r| r.project_id).collect();
    let cyclo: Vec<i32> = rows.iter().map(|r| r.cyclomatic).collect();
    let cogn: Vec<i32> = rows.iter().map(|r| r.cognitive).collect();
    let h_n1: Vec<i32> = rows.iter().map(|r| r.halstead_n1).collect();
    let h_n2: Vec<i32> = rows.iter().map(|r| r.halstead_n2).collect();
    let h_bn1: Vec<i32> = rows.iter().map(|r| r.halstead_big_n1).collect();
    let h_bn2: Vec<i32> = rows.iter().map(|r| r.halstead_big_n2).collect();
    let h_v: Vec<f64> = rows.iter().map(|r| r.halstead_volume).collect();
    let h_d: Vec<f64> = rows.iter().map(|r| r.halstead_difficulty).collect();
    let h_e: Vec<f64> = rows.iter().map(|r| r.halstead_effort).collect();
    let h_b: Vec<f64> = rows.iter().map(|r| r.halstead_bugs).collect();
    let np: Vec<i64> = rows.iter().map(|r| r.npath).collect();
    let np_ovf: Vec<bool> = rows.iter().map(|r| r.npath_overflow).collect();
    let loc: Vec<i32> = rows.iter().map(|r| r.loc).collect();
    let cl: Vec<i32> = rows.iter().map(|r| r.comment_lines).collect();
    let mi: Vec<f64> = rows.iter().map(|r| r.maintainability_index).collect();
    let panic_p: Vec<i32> = rows.iter().map(|r| r.panic_paths).collect();
    let uns: Vec<i32> = rows.iter().map(|r| r.unsafe_blocks).collect();

    let res = sqlx::query(
        "INSERT INTO function_metrics (
            function_id, file_id, project_id,
            cyclomatic, cognitive,
            halstead_n1, halstead_n2, halstead_big_n1, halstead_big_n2,
            halstead_volume, halstead_difficulty, halstead_effort, halstead_bugs,
            npath, npath_overflow,
            loc, comment_lines,
            maintainability_index,
            panic_paths, unsafe_blocks,
            computed_at
        )
        SELECT * FROM UNNEST(
            $1::int8[], $2::int8[], $3::int4[],
            $4::int4[], $5::int4[],
            $6::int4[], $7::int4[], $8::int4[], $9::int4[],
            $10::float8[], $11::float8[], $12::float8[], $13::float8[],
            $14::int8[], $15::bool[],
            $16::int4[], $17::int4[],
            $18::float8[],
            $19::int4[], $20::int4[]
        ) AS u(
            function_id, file_id, project_id,
            cyclomatic, cognitive,
            halstead_n1, halstead_n2, halstead_big_n1, halstead_big_n2,
            halstead_volume, halstead_difficulty, halstead_effort, halstead_bugs,
            npath, npath_overflow,
            loc, comment_lines,
            maintainability_index,
            panic_paths, unsafe_blocks
        ), (SELECT NOW())
        ON CONFLICT (function_id) DO UPDATE SET
            file_id = EXCLUDED.file_id,
            project_id = EXCLUDED.project_id,
            cyclomatic = EXCLUDED.cyclomatic,
            cognitive = EXCLUDED.cognitive,
            halstead_n1 = EXCLUDED.halstead_n1,
            halstead_n2 = EXCLUDED.halstead_n2,
            halstead_big_n1 = EXCLUDED.halstead_big_n1,
            halstead_big_n2 = EXCLUDED.halstead_big_n2,
            halstead_volume = EXCLUDED.halstead_volume,
            halstead_difficulty = EXCLUDED.halstead_difficulty,
            halstead_effort = EXCLUDED.halstead_effort,
            halstead_bugs = EXCLUDED.halstead_bugs,
            npath = EXCLUDED.npath,
            npath_overflow = EXCLUDED.npath_overflow,
            loc = EXCLUDED.loc,
            comment_lines = EXCLUDED.comment_lines,
            maintainability_index = EXCLUDED.maintainability_index,
            panic_paths = EXCLUDED.panic_paths,
            unsafe_blocks = EXCLUDED.unsafe_blocks,
            computed_at = NOW()",
    )
    .bind(&function_ids)
    .bind(&file_ids)
    .bind(&project_ids)
    .bind(&cyclo)
    .bind(&cogn)
    .bind(&h_n1)
    .bind(&h_n2)
    .bind(&h_bn1)
    .bind(&h_bn2)
    .bind(&h_v)
    .bind(&h_d)
    .bind(&h_e)
    .bind(&h_b)
    .bind(&np)
    .bind(&np_ovf)
    .bind(&loc)
    .bind(&cl)
    .bind(&mi)
    .bind(&panic_p)
    .bind(&uns)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Read the function-metrics watermark for a project.
pub async fn get_function_metrics_watermark(
    pool: &PgPool,
    project_id: i32,
) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
    let key = format!("function_metrics_last_run:{}", project_id);
    let val: Option<String> = sqlx::query_scalar("SELECT value FROM pgmcp_metadata WHERE key = $1")
        .bind(&key)
        .fetch_optional(pool)
        .await?;
    Ok(val.and_then(|s| {
        DateTime::parse_from_rfc3339(&s)
            .ok()
            .map(|dt| dt.with_timezone(&Utc))
    }))
}

/// Set the function-metrics watermark for a project.
pub async fn set_function_metrics_watermark(
    pool: &PgPool,
    project_id: i32,
    ts: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    let key = format!("function_metrics_last_run:{}", project_id);
    sqlx::query(
        "INSERT INTO pgmcp_metadata (key, value) VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(&key)
    .bind(ts.to_rfc3339())
    .execute(pool)
    .await?;
    Ok(())
}

// ----------------------------------------------------------------------------
// Call-graph cron support
// ----------------------------------------------------------------------------

/// One node in the in-process call graph (one row per function symbol in a
/// project). Returned by `list_function_nodes_for_project`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FunctionNodeRow {
    pub symbol_id: i64,
    pub file_id: i64,
    pub name: String,
    pub relative_path: String,
    pub language: String,
    pub parent_id: Option<i64>,
}

/// Fetch every function symbol in a project, with the file path/language and
/// its parent_id (so the call-graph builder can decide `is_method`).
pub async fn list_function_nodes_for_project(
    pool: &PgPool,
    project_id: i32,
) -> Result<Vec<FunctionNodeRow>, sqlx::Error> {
    sqlx::query_as::<_, FunctionNodeRow>(
        "SELECT fs.id as symbol_id,
                fs.file_id,
                fs.name,
                f.relative_path,
                f.language,
                fs.parent_id
         FROM file_symbols fs
         JOIN indexed_files f ON fs.file_id = f.id
         WHERE f.project_id = $1 AND fs.kind = 'function'",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
}

/// One raw call edge for the call-graph cron — sourced from `symbol_references`
/// rows where `ref_kind='call'` and the source is inside a known function.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RawCallEdgeRow {
    pub source_file_id: i64,
    pub source_symbol_id: Option<i64>,
    pub target_file_id: Option<i64>,
    pub target_symbol_id: Option<i64>,
    pub target_raw: String,
}

/// Read all call-kind symbol_references for a project.
pub async fn list_call_edges_for_project(
    pool: &PgPool,
    project_id: i32,
) -> Result<Vec<RawCallEdgeRow>, sqlx::Error> {
    sqlx::query_as::<_, RawCallEdgeRow>(
        "SELECT sr.source_file_id,
                sr.source_symbol_id,
                sr.target_file_id,
                sr.target_symbol_id,
                sr.target_raw
         FROM symbol_references sr
         JOIN indexed_files f ON sr.source_file_id = f.id
         WHERE f.project_id = $1
           AND sr.ref_kind = 'call'
           AND sr.source_symbol_id IS NOT NULL",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
}

/// Delete existing call edges for a project before re-populating.
pub async fn delete_call_edges_for_project(
    pool: &PgPool,
    project_id: i32,
) -> Result<u64, sqlx::Error> {
    let res =
        sqlx::query("DELETE FROM code_graph_edges WHERE project_id = $1 AND edge_type = 'call'")
            .bind(project_id)
            .execute(pool)
            .await?;
    Ok(res.rows_affected())
}

/// Bulk-insert call edges into `code_graph_edges` with `edge_type='call'`.
/// Skips rows whose source_symbol_id is NULL (would violate CHECK constraint).
pub async fn bulk_insert_call_edges(
    pool: &PgPool,
    project_id: i32,
    edges: &[RawCallEdgeRow],
) -> Result<u64, sqlx::Error> {
    if edges.is_empty() {
        return Ok(0);
    }
    let valid: Vec<&RawCallEdgeRow> = edges
        .iter()
        .filter(|e| e.source_symbol_id.is_some())
        .collect();
    if valid.is_empty() {
        return Ok(0);
    }
    let project_ids: Vec<i32> = vec![project_id; valid.len()];
    let source_files: Vec<i64> = valid.iter().map(|e| e.source_file_id).collect();
    let target_files: Vec<Option<i64>> = valid.iter().map(|e| e.target_file_id).collect();
    let source_symbols: Vec<i64> = valid
        .iter()
        .map(|e| e.source_symbol_id.expect("filtered above"))
        .collect();
    let target_symbols: Vec<Option<i64>> = valid.iter().map(|e| e.target_symbol_id).collect();
    let target_raws: Vec<String> = valid.iter().map(|e| e.target_raw.clone()).collect();

    let res = sqlx::query(
        "INSERT INTO code_graph_edges
            (project_id, source_file_id, target_file_id, source_symbol_id,
             target_symbol_id, edge_type, target_raw, weight, computed_at)
         SELECT u.project_id, u.source_file_id, u.target_file_id, u.source_symbol_id,
                u.target_symbol_id, 'call', u.target_raw, 1.0, NOW()
         FROM UNNEST(
             $1::int4[], $2::int8[], $3::int8[], $4::int8[],
             $5::int8[], $6::text[]
         ) AS u(project_id, source_file_id, target_file_id, source_symbol_id,
                target_symbol_id, target_raw)
         ON CONFLICT (source_file_id, COALESCE(target_file_id, -1::BIGINT), edge_type, COALESCE(target_raw, '')) DO NOTHING",
    )
    .bind(&project_ids)
    .bind(&source_files)
    .bind(&target_files)
    .bind(&source_symbols)
    .bind(&target_symbols)
    .bind(&target_raws)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Update `function_metrics.fan_in` / `fan_out` for a batch of function IDs.
/// Rows whose function_id has no row in `function_metrics` are silently
/// ignored (their metrics row hasn't been computed yet; the next
/// function-metrics cron pass will populate it).
pub async fn update_function_fan_io(
    pool: &PgPool,
    triples: &[(i64, i32, i32)], // (function_id, fan_in, fan_out)
) -> Result<u64, sqlx::Error> {
    if triples.is_empty() {
        return Ok(0);
    }
    let ids: Vec<i64> = triples.iter().map(|(i, _, _)| *i).collect();
    let fis: Vec<i32> = triples.iter().map(|(_, fi, _)| *fi).collect();
    let fos: Vec<i32> = triples.iter().map(|(_, _, fo)| *fo).collect();
    let res = sqlx::query(
        "UPDATE function_metrics
         SET fan_in = u.fan_in, fan_out = u.fan_out
         FROM UNNEST($1::int8[], $2::int4[], $3::int4[]) AS u(function_id, fan_in, fan_out)
         WHERE function_metrics.function_id = u.function_id",
    )
    .bind(&ids)
    .bind(&fis)
    .bind(&fos)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Bulk-update the call-graph-derived centrality columns on `function_metrics`,
/// keyed by `function_id` (= `file_symbols.id` = the call graph's `symbol_id`).
/// Owned by the call-graph cron; `upsert_function_metrics_batch` deliberately
/// omits these columns from its ON CONFLICT clause so a metrics pass never
/// resets them. Mirrors `update_function_fan_io`'s UNNEST shape.
#[allow(clippy::type_complexity)]
pub async fn update_function_centralities(
    pool: &PgPool,
    // (function_id, pagerank, betweenness, community_id, coreness, harmonic)
    rows: &[(i64, f64, f64, i32, i32, f64)],
) -> Result<u64, sqlx::Error> {
    if rows.is_empty() {
        return Ok(0);
    }
    let ids: Vec<i64> = rows.iter().map(|r| r.0).collect();
    let pr: Vec<f64> = rows.iter().map(|r| r.1).collect();
    let btw: Vec<f64> = rows.iter().map(|r| r.2).collect();
    let comm: Vec<i32> = rows.iter().map(|r| r.3).collect();
    let core: Vec<i32> = rows.iter().map(|r| r.4).collect();
    let harm: Vec<f64> = rows.iter().map(|r| r.5).collect();
    let res = sqlx::query(
        "UPDATE function_metrics
         SET pagerank = u.pagerank,
             betweenness = u.betweenness,
             community_id = u.community_id,
             coreness = u.coreness,
             harmonic = u.harmonic
         FROM UNNEST($1::int8[], $2::float8[], $3::float8[], $4::int4[], $5::int4[], $6::float8[])
              AS u(function_id, pagerank, betweenness, community_id, coreness, harmonic)
         WHERE function_metrics.function_id = u.function_id",
    )
    .bind(&ids)
    .bind(&pr)
    .bind(&btw)
    .bind(&comm)
    .bind(&core)
    .bind(&harm)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Per-file aggregate of the rigorous per-function `function_metrics` (real AST
/// cyclomatic / cognitive / Halstead / Maintainability-Index). Lets
/// `design_metrics` and `complexity_hotspots` emit AST-grade values for parsed
/// files instead of their regex/line-count heuristics. `sum_cyclomatic` is the
/// true Chidamber-Kemerer WMC (Σ method complexity); `max_cyclomatic` is the
/// file's worst single function.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FileFunctionAggregate {
    pub file_id: i64,
    pub function_count: i64,
    pub sum_cyclomatic: i64,
    pub max_cyclomatic: i32,
    pub sum_cognitive: i64,
    pub sum_halstead_volume: f64,
    pub avg_maintainability: f64,
    pub min_maintainability: f64,
}

/// Aggregate `function_metrics` per file for a project. Files with no parsed
/// functions simply don't appear (callers fall back to their heuristic).
pub async fn get_file_function_metric_aggregates(
    pool: &PgPool,
    project_id: i32,
) -> Result<Vec<FileFunctionAggregate>, sqlx::Error> {
    sqlx::query_as::<_, FileFunctionAggregate>(
        "SELECT fm.file_id,
                COUNT(*)                                      AS function_count,
                COALESCE(SUM(fm.cyclomatic), 0)               AS sum_cyclomatic,
                COALESCE(MAX(fm.cyclomatic), 0)               AS max_cyclomatic,
                COALESCE(SUM(fm.cognitive), 0)                AS sum_cognitive,
                COALESCE(SUM(fm.halstead_volume), 0.0)        AS sum_halstead_volume,
                COALESCE(AVG(fm.maintainability_index), 100.0) AS avg_maintainability,
                COALESCE(MIN(fm.maintainability_index), 100.0) AS min_maintainability
         FROM function_metrics fm
         WHERE fm.project_id = $1
         GROUP BY fm.file_id",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
}

/// Per-file AST-complexity summary keyed by `relative_path` (for tools that
/// work in path space, e.g. `complexity_hotspots`): `(relative_path,
/// max_cyclomatic, min_maintainability, function_count)`. Files with no parsed
/// functions are absent.
pub async fn get_file_ast_complexity_by_path(
    pool: &PgPool,
    project_id: i32,
) -> Result<Vec<(String, i32, f64, i64)>, sqlx::Error> {
    sqlx::query_as::<_, (String, i32, f64, i64)>(
        "SELECT f.relative_path,
                COALESCE(MAX(fm.cyclomatic), 0)                AS max_cyclomatic,
                COALESCE(MIN(fm.maintainability_index), 100.0) AS min_maintainability,
                COUNT(*)                                       AS function_count
         FROM function_metrics fm
         JOIN indexed_files f ON f.id = fm.file_id
         WHERE fm.project_id = $1
         GROUP BY f.relative_path",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
}

/// Read the call-graph watermark for a project.
pub async fn get_call_graph_watermark(
    pool: &PgPool,
    project_id: i32,
) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
    let key = format!("call_graph_last_run:{}", project_id);
    let val: Option<String> = sqlx::query_scalar("SELECT value FROM pgmcp_metadata WHERE key = $1")
        .bind(&key)
        .fetch_optional(pool)
        .await?;
    Ok(val.and_then(|s| {
        DateTime::parse_from_rfc3339(&s)
            .ok()
            .map(|dt| dt.with_timezone(&Utc))
    }))
}

/// Set the call-graph watermark for a project.
pub async fn set_call_graph_watermark(
    pool: &PgPool,
    project_id: i32,
    ts: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    let key = format!("call_graph_last_run:{}", project_id);
    sqlx::query(
        "INSERT INTO pgmcp_metadata (key, value) VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(&key)
    .bind(ts.to_rfc3339())
    .execute(pool)
    .await?;
    Ok(())
}
