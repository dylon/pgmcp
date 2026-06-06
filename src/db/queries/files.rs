//! Indexed-file queries (metadata/upsert/content-hash/read/tree/path
//! resolution/duplicate handling/stale cleanup). Extracted from `queries.rs` (god-file split).
#![allow(unused_imports)]

use crate::db::queries::*;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

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

/// Delete every indexed file of a given language (and its chunks), so the
/// background scanner re-reads and re-extracts only those files on its next
/// pass. The narrow mechanism for re-applying an extractor change (e.g. the
/// LaTeX pandoc→in-process cutover) WITHOUT a global rescan: every other file's
/// Level-1 size+mtime skip is preserved. Two-step (chunks then files), mirroring
/// `delete_file`, so correctness does not depend on a FK cascade. Index-backed
/// by `idx_files_language`. Returns the number of file rows removed.
pub async fn delete_files_by_language(pool: &PgPool, language: &str) -> Result<u64, sqlx::Error> {
    sqlx::query(
        "DELETE FROM file_chunks WHERE file_id IN \
         (SELECT id FROM indexed_files WHERE language = $1)",
    )
    .bind(language)
    .execute(pool)
    .await?;
    let result = sqlx::query("DELETE FROM indexed_files WHERE language = $1")
        .bind(language)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
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
        "SELECT f.path, f.relative_path, f.language, f.size_bytes, f.line_count,
                f.truncated, f.indexed_at, f.modified_at, p.name AS project_name
         FROM indexed_files f
         LEFT JOIN projects p ON p.id = f.project_id
         WHERE f.path = $1",
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
    pub project_name: Option<String>,
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
    let rows: Vec<(i64, Option<String>)> = sqlx::query_as(
        "WITH matching_projects AS (
             SELECT id
             FROM projects
             WHERE name = $1
         ),
         project_match AS (
             SELECT COUNT(*)::int8 AS match_count, MIN(id) AS project_id
             FROM matching_projects
         )
         SELECT pm.match_count, f.relative_path
         FROM project_match pm
         LEFT JOIN indexed_files f
           ON pm.match_count = 1 AND f.project_id = pm.project_id
         ORDER BY f.relative_path NULLS LAST",
    )
    .bind(project_name)
    .fetch_all(pool)
    .await?;

    let Some((match_count, _)) = rows.first() else {
        return Ok(Vec::new());
    };
    if *match_count > 1 {
        return Err(sqlx::Error::Protocol(format!(
            "ambiguous project name '{project_name}' matched {match_count} indexed projects"
        )));
    }
    if *match_count == 0 {
        return Ok(Vec::new());
    }

    let paths: Vec<String> = rows.into_iter().filter_map(|(_, path)| path).collect();

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

/// Get the file_id for a given absolute path.
pub async fn get_file_id_by_path(pool: &PgPool, path: &str) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar::<_, i64>("SELECT id FROM indexed_files WHERE path = $1")
        .bind(path)
        .fetch_optional(pool)
        .await
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
            "WITH matching_projects AS (
                 SELECT id, name
                 FROM projects
                 WHERE name = $1
             ),
             unique_project AS (
                 SELECT id, name
                 FROM matching_projects
                 WHERE (SELECT COUNT(*) FROM matching_projects) = 1
             )
             SELECT f.id as file_id, f.path, f.relative_path, f.language, f.line_count,
                    p.id as project_id, p.name as project_name
             FROM indexed_files f
             JOIN unique_project p ON p.id = f.project_id
             WHERE f.relative_path = $2",
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
