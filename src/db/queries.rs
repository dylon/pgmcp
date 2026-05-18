//! Database query functions.

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
    let row = sqlx::query_scalar::<_, i32>(
        "INSERT INTO projects (workspace_path, path, name, git_common_dir, git_root_commits)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (path) DO UPDATE SET
            workspace_path = EXCLUDED.workspace_path,
            name = EXCLUDED.name,
            git_common_dir = EXCLUDED.git_common_dir,
            git_root_commits = EXCLUDED.git_root_commits
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
    let embedding_vec = pgvector::Vector::from(embedding.to_vec());
    sqlx::query(
        "INSERT INTO file_chunks (file_id, chunk_index, content, start_line, end_line, embedding)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (file_id, chunk_index) DO UPDATE SET
            content = EXCLUDED.content,
            start_line = EXCLUDED.start_line,
            end_line = EXCLUDED.end_line,
            embedding = EXCLUDED.embedding",
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
                        1 - (c.embedding <=> $1) as score,
                        p.name as project_name
                 FROM file_chunks c
                 JOIN indexed_files f ON f.id = c.file_id
                 JOIN projects p ON p.id = f.project_id
                 WHERE f.language = $3 AND p.name = $4
                   AND {}
                 ORDER BY c.embedding <=> $1
                 LIMIT $2",
                worktree_dedup_clause(5)
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
                        1 - (c.embedding <=> $1) as score,
                        p.name as project_name
                 FROM file_chunks c
                 JOIN indexed_files f ON f.id = c.file_id
                 JOIN projects p ON p.id = f.project_id
                 WHERE f.language = $3
                   AND {}
                 ORDER BY c.embedding <=> $1
                 LIMIT $2",
                worktree_dedup_clause(4)
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
                        1 - (c.embedding <=> $1) as score,
                        p.name as project_name
                 FROM file_chunks c
                 JOIN indexed_files f ON f.id = c.file_id
                 JOIN projects p ON p.id = f.project_id
                 WHERE p.name = $3
                   AND {}
                 ORDER BY c.embedding <=> $1
                 LIMIT $2",
                worktree_dedup_clause(4)
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
                        1 - (c.embedding <=> $1) as score,
                        p.name as project_name
                 FROM file_chunks c
                 JOIN indexed_files f ON f.id = c.file_id
                 JOIN projects p ON p.id = f.project_id
                 WHERE {}
                 ORDER BY c.embedding <=> $1
                 LIMIT $2",
                worktree_dedup_clause(3)
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
    let column = match embedding.len() {
        384 => "embedding",
        1024 => "embedding_v2",
        other => {
            return Err(sqlx::Error::Protocol(format!(
                "recall_prompts: unsupported query-embedding dim {} \
                 (expected 384 for MiniLM or 1024 for BGE-M3)",
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
// Statistics queries
// ============================================================================

/// Count total indexed files.
pub async fn count_indexed_files(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM indexed_files")
        .fetch_one(pool)
        .await?;
    Ok(count as u64)
}

/// Count total chunks.
pub async fn count_chunks(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM file_chunks")
        .fetch_one(pool)
        .await?;
    Ok(count as u64)
}

/// Count total projects.
pub async fn count_projects(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM projects")
        .fetch_one(pool)
        .await?;
    Ok(count as u64)
}

/// Get total bytes indexed.
pub async fn total_bytes_indexed(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let total = sqlx::query_scalar::<_, Option<i64>>("SELECT SUM(size_bytes) FROM indexed_files")
        .fetch_one(pool)
        .await?;
    Ok(total.unwrap_or(0) as u64)
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

/// Insert a git commit chunk with its embedding.
pub async fn insert_git_commit_chunk(
    pool: &PgPool,
    commit_id: i64,
    chunk_index: i32,
    content: &str,
    embedding: &[f32],
) -> Result<(), sqlx::Error> {
    let embedding_vec = pgvector::Vector::from(embedding.to_vec());
    sqlx::query(
        "INSERT INTO git_commit_chunks (commit_id, chunk_index, content, embedding)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (commit_id, chunk_index) DO UPDATE SET
            content = EXCLUDED.content,
            embedding = EXCLUDED.embedding",
    )
    .bind(commit_id)
    .bind(chunk_index)
    .bind(content)
    .bind(embedding_vec)
    .execute(pool)
    .await?;
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

/// Semantic search across git commit chunks.
pub async fn semantic_search_commits(
    pool: &PgPool,
    embedding: &[f32],
    limit: i32,
    project: Option<&str>,
    ef_search: i32,
) -> Result<Vec<CommitSearchResult>, sqlx::Error> {
    let embedding_vec = pgvector::Vector::from(embedding.to_vec());

    let mut tx = pool.begin().await?;
    sqlx::query(&format!("SET LOCAL hnsw.ef_search = {}", ef_search))
        .execute(&mut *tx)
        .await?;

    let results = if let Some(proj) = project {
        sqlx::query_as::<_, CommitSearchResult>(
            "SELECT g.commit_hash, g.author, g.author_date, g.subject,
                    cc.content as chunk_content,
                    1 - (cc.embedding <=> $1) as score,
                    p.name as project_name
             FROM git_commit_chunks cc
             JOIN git_commits g ON g.id = cc.commit_id
             JOIN projects p ON p.id = g.project_id
             WHERE p.name = $3
             ORDER BY cc.embedding <=> $1
             LIMIT $2",
        )
        .bind(&embedding_vec)
        .bind(limit)
        .bind(proj)
        .fetch_all(&mut *tx)
        .await?
    } else {
        sqlx::query_as::<_, CommitSearchResult>(
            "SELECT g.commit_hash, g.author, g.author_date, g.subject,
                    cc.content as chunk_content,
                    1 - (cc.embedding <=> $1) as score,
                    p.name as project_name
             FROM git_commit_chunks cc
             JOIN git_commits g ON g.id = cc.commit_id
             JOIN projects p ON p.id = g.project_id
             ORDER BY cc.embedding <=> $1
             LIMIT $2",
        )
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
        // Normalize so chunk_id_a < chunk_id_b
        let (cid_a, fid_a, pid_a, pa, pna, cid_b, fid_b, pid_b, pb, pnb) =
            if row.chunk_id_a < row.chunk_id_b {
                (
                    row.chunk_id_a,
                    row.file_id_a,
                    row.project_id_a,
                    &row.path_a,
                    &row.project_name_a,
                    row.chunk_id_b,
                    row.file_id_b,
                    row.project_id_b,
                    &row.path_b,
                    &row.project_name_b,
                )
            } else {
                (
                    row.chunk_id_b,
                    row.file_id_b,
                    row.project_id_b,
                    &row.path_b,
                    &row.project_name_b,
                    row.chunk_id_a,
                    row.file_id_a,
                    row.project_id_a,
                    &row.path_a,
                    &row.project_name_a,
                )
            };

        let result = sqlx::query(
            "INSERT INTO cross_project_similarities
                (chunk_id_a, file_id_a, project_id_a, chunk_id_b, file_id_b, project_id_b,
                 chunk_similarity, path_a, path_b, project_name_a, project_name_b, language)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
             ON CONFLICT (chunk_id_a, chunk_id_b) DO UPDATE SET
                chunk_similarity = GREATEST(cross_project_similarities.chunk_similarity, EXCLUDED.chunk_similarity)"
        )
        .bind(cid_a)
        .bind(fid_a)
        .bind(pid_a)
        .bind(cid_b)
        .bind(fid_b)
        .bind(pid_b)
        .bind(row.similarity)
        .bind(pa)
        .bind(pna)
        .bind(pb)
        .bind(pnb)
        .bind(&row.language)
        .execute(pool)
        .await?;

        inserted += result.rows_affected();
    }

    Ok(inserted)
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
                    f.path, f.language, c.content, c.embedding::real[] as embedding
             FROM file_chunks c
             JOIN indexed_files f ON f.id = c.file_id
             JOIN projects p ON p.id = f.project_id
             WHERE f.language = $1
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
                    f.path, f.language, c.content, c.embedding::real[] as embedding
             FROM file_chunks c
             JOIN indexed_files f ON f.id = c.file_id
             JOIN projects p ON p.id = f.project_id
             WHERE {canonical_filter}
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
    if let Some(lang) = language {
        let rows = sqlx::query_as::<_, BulkChunkRow>(
            "SELECT c.id as chunk_id, c.file_id, f.project_id, p.name as project_name,
                    f.path, f.language, c.content, c.embedding::real[] as embedding
             FROM file_chunks c
             JOIN indexed_files f ON f.id = c.file_id
             JOIN projects p ON p.id = f.project_id
             WHERE p.name = $1 AND f.language = $2
             ORDER BY c.id",
        )
        .bind(project_name)
        .bind(lang)
        .fetch_all(pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    } else {
        let rows = sqlx::query_as::<_, BulkChunkRow>(
            "SELECT c.id as chunk_id, c.file_id, f.project_id, p.name as project_name,
                    f.path, f.language, c.content, c.embedding::real[] as embedding
             FROM file_chunks c
             JOIN indexed_files f ON f.id = c.file_id
             JOIN projects p ON p.id = f.project_id
             WHERE p.name = $1
             ORDER BY c.id",
        )
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
    if let Some(proj) = project {
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
        .fetch_all(pool)
        .await
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
        .fetch_all(pool)
        .await
    }
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

    // Generate a per-batch ordinal so RETURNING comes back in input order
    // even when ON CONFLICT DO UPDATE fires.
    let ordinals: Vec<i32> = (0..symbols.len() as i32).collect();

    let rows: Vec<(i32, i64)> = sqlx::query_as::<_, (i32, i64)>(
        "WITH input AS (
             SELECT * FROM UNNEST(
                 $1::int4[], $2::int8[], $3::text[], $4::text[],
                 $5::int4[], $6::int4[], $7::text[], $8::text[]
             ) AS u(ord, file_id, name, kind, start_line, end_line, visibility, signature)
         ),
         inserted AS (
             INSERT INTO file_symbols (file_id, name, kind, start_line, end_line, visibility, signature)
             SELECT file_id, name, kind, start_line, end_line, visibility, signature
             FROM input
             ON CONFLICT (file_id, kind, name, start_line) DO UPDATE SET
                 end_line = EXCLUDED.end_line,
                 visibility = EXCLUDED.visibility,
                 signature = EXCLUDED.signature
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
    let res = sqlx::query(
        "UPDATE symbol_references sr
         SET target_symbol_id = fs.id, target_file_id = fs.file_id
         FROM file_symbols fs
         JOIN indexed_files src_f ON src_f.id = sr.source_file_id
         JOIN indexed_files tgt_f ON tgt_f.id = fs.file_id
         WHERE src_f.project_id = $1
           AND tgt_f.project_id = $1
           AND sr.target_symbol_id IS NULL
           AND sr.target_raw = fs.name",
    )
    .bind(project_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
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
