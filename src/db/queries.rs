//! Database query functions.

use sqlx::PgPool;
use chrono::{DateTime, Utc};

// ============================================================================
// Project queries
// ============================================================================

/// Upsert a project (create or update).
pub async fn upsert_project(
    pool: &PgPool,
    workspace_path: &str,
    path: &str,
    name: &str,
) -> Result<i32, sqlx::Error> {
    let row = sqlx::query_scalar::<_, i32>(
        "INSERT INTO projects (workspace_path, path, name)
         VALUES ($1, $2, $3)
         ON CONFLICT (path) DO UPDATE SET
            workspace_path = EXCLUDED.workspace_path,
            name = EXCLUDED.name
         RETURNING id"
    )
    .bind(workspace_path)
    .bind(path)
    .bind(name)
    .fetch_one(pool)
    .await?;

    Ok(row)
}

/// List all projects with file counts.
pub async fn list_projects(
    pool: &PgPool,
) -> Result<Vec<ProjectInfo>, sqlx::Error> {
    let rows = sqlx::query_as::<_, ProjectInfo>(
        "SELECT p.id, p.workspace_path, p.path, p.name, p.discovered_at, p.last_scanned_at,
                COUNT(f.id) as file_count
         FROM projects p
         LEFT JOIN indexed_files f ON f.project_id = p.id
         GROUP BY p.id
         ORDER BY p.name"
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
    pub discovered_at: Option<DateTime<Utc>>,
    pub last_scanned_at: Option<DateTime<Utc>>,
    pub file_count: Option<i64>,
}

/// Find the project whose path is the longest prefix of a given directory.
/// Used by the `context` CLI subcommand to identify which project the user is in.
pub async fn find_project_by_cwd(pool: &PgPool, cwd: &str) -> Result<Option<ProjectInfo>, sqlx::Error> {
    sqlx::query_as::<_, ProjectInfo>(
        "SELECT p.id, p.workspace_path, p.path, p.name, p.discovered_at, p.last_scanned_at,
                (SELECT COUNT(*) FROM indexed_files f WHERE f.project_id = p.id) AS file_count
         FROM projects p
         WHERE $1 LIKE p.path || '%'
         ORDER BY LENGTH(p.path) DESC
         LIMIT 1"
    )
    .bind(cwd)
    .fetch_optional(pool)
    .await
}

/// Returns language breakdown (language, count) for a project, ordered by count descending.
pub async fn language_summary(pool: &PgPool, project_name: &str) -> Result<Vec<LanguageCount>, sqlx::Error> {
    sqlx::query_as::<_, LanguageCount>(
        "SELECT f.language, COUNT(*) as count
         FROM indexed_files f
         JOIN projects p ON f.project_id = p.id
         WHERE p.name = $1
         GROUP BY f.language
         ORDER BY count DESC"
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
        "SELECT path, modified_at, size_bytes FROM indexed_files WHERE content_hash IS NOT NULL"
    )
    .fetch_all(pool)
    .await
}

// ============================================================================
// File queries
// ============================================================================

/// Upsert an indexed file.
/// Pass `content_hash: None` during initial insert (deferred commit);
/// the real hash is set via `finalize_file_hash` after all chunks are inserted.
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
    modified_at: DateTime<Utc>,
) -> Result<i64, sqlx::Error> {
    let row = sqlx::query_scalar::<_, i64>(
        "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, content_hash, line_count, truncated, modified_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
         ON CONFLICT (path) DO UPDATE SET
            project_id = EXCLUDED.project_id,
            relative_path = EXCLUDED.relative_path,
            language = EXCLUDED.language,
            size_bytes = EXCLUDED.size_bytes,
            content = EXCLUDED.content,
            content_hash = EXCLUDED.content_hash,
            line_count = EXCLUDED.line_count,
            truncated = EXCLUDED.truncated,
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
    .bind(modified_at)
    .fetch_one(pool)
    .await?;

    Ok(row)
}

/// Get the content hash for a file path (for skip-if-unchanged check).
/// Returns `None` if the file is not indexed or has a NULL hash (incomplete indexing).
pub async fn get_content_hash(pool: &PgPool, path: &str) -> Result<Option<i64>, sqlx::Error> {
    let row: Option<Option<i64>> = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT content_hash FROM indexed_files WHERE path = $1"
    )
    .bind(path)
    .fetch_optional(pool)
    .await?;

    // flatten: no row → None, row with NULL hash → None, row with hash → Some(hash)
    Ok(row.flatten())
}

/// Finalize a file's content hash after all chunks have been inserted.
/// This completes the two-phase commit: the file is now fully indexed.
pub async fn finalize_file_hash(pool: &PgPool, file_id: i64, content_hash: i64) -> Result<(), sqlx::Error> {
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
            embedding = EXCLUDED.embedding"
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
}

/// Semantic search using vector similarity.
///
/// Sets `hnsw.ef_search` on the connection for improved recall before executing
/// the k-NN query. Supports optional filtering by language and/or project name.
pub async fn semantic_search(
    pool: &PgPool,
    embedding: &[f32],
    limit: i32,
    language: Option<&str>,
    project: Option<&str>,
    ef_search: i32,
) -> Result<Vec<SearchResult>, sqlx::Error> {
    let embedding_vec = pgvector::Vector::from(embedding.to_vec());

    // Acquire a dedicated connection so ef_search applies to our query.
    // Using SET LOCAL within a transaction keeps it scoped to this operation.
    let mut tx = pool.begin().await?;

    sqlx::query(&format!("SET LOCAL hnsw.ef_search = {}", ef_search))
        .execute(&mut *tx)
        .await?;

    // Build the query dynamically based on which filters are present
    let results = match (language, project) {
        (Some(lang), Some(proj)) => {
            sqlx::query_as::<_, SearchResult>(
                "SELECT f.path, f.relative_path, f.language,
                        c.content as chunk_content, c.start_line, c.end_line,
                        1 - (c.embedding <=> $1) as score
                 FROM file_chunks c
                 JOIN indexed_files f ON f.id = c.file_id
                 JOIN projects p ON p.id = f.project_id
                 WHERE f.language = $3 AND p.name = $4
                 ORDER BY c.embedding <=> $1
                 LIMIT $2"
            )
            .bind(&embedding_vec)
            .bind(limit)
            .bind(lang)
            .bind(proj)
            .fetch_all(&mut *tx)
            .await?
        }
        (Some(lang), None) => {
            sqlx::query_as::<_, SearchResult>(
                "SELECT f.path, f.relative_path, f.language,
                        c.content as chunk_content, c.start_line, c.end_line,
                        1 - (c.embedding <=> $1) as score
                 FROM file_chunks c
                 JOIN indexed_files f ON f.id = c.file_id
                 WHERE f.language = $3
                 ORDER BY c.embedding <=> $1
                 LIMIT $2"
            )
            .bind(&embedding_vec)
            .bind(limit)
            .bind(lang)
            .fetch_all(&mut *tx)
            .await?
        }
        (None, Some(proj)) => {
            sqlx::query_as::<_, SearchResult>(
                "SELECT f.path, f.relative_path, f.language,
                        c.content as chunk_content, c.start_line, c.end_line,
                        1 - (c.embedding <=> $1) as score
                 FROM file_chunks c
                 JOIN indexed_files f ON f.id = c.file_id
                 JOIN projects p ON p.id = f.project_id
                 WHERE p.name = $3
                 ORDER BY c.embedding <=> $1
                 LIMIT $2"
            )
            .bind(&embedding_vec)
            .bind(limit)
            .bind(proj)
            .fetch_all(&mut *tx)
            .await?
        }
        (None, None) => {
            sqlx::query_as::<_, SearchResult>(
                "SELECT f.path, f.relative_path, f.language,
                        c.content as chunk_content, c.start_line, c.end_line,
                        1 - (c.embedding <=> $1) as score
                 FROM file_chunks c
                 JOIN indexed_files f ON f.id = c.file_id
                 ORDER BY c.embedding <=> $1
                 LIMIT $2"
            )
            .bind(&embedding_vec)
            .bind(limit)
            .fetch_all(&mut *tx)
            .await?
        }
    };

    tx.commit().await?;

    Ok(results)
}

/// Full-text search using PostgreSQL tsvector/tsquery.
pub async fn text_search(
    pool: &PgPool,
    query: &str,
    limit: i32,
    language: Option<&str>,
) -> Result<Vec<TextSearchResult>, sqlx::Error> {
    let results = if let Some(lang) = language {
        sqlx::query_as::<_, TextSearchResult>(
            "SELECT path, relative_path, language, content,
                    ts_rank(to_tsvector('english', content), plainto_tsquery('english', $1)) as rank
             FROM indexed_files
             WHERE to_tsvector('english', content) @@ plainto_tsquery('english', $1)
               AND language = $3
             ORDER BY rank DESC
             LIMIT $2"
        )
        .bind(query)
        .bind(limit)
        .bind(lang)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query_as::<_, TextSearchResult>(
            "SELECT path, relative_path, language, content,
                    ts_rank(to_tsvector('english', content), plainto_tsquery('english', $1)) as rank
             FROM indexed_files
             WHERE to_tsvector('english', content) @@ plainto_tsquery('english', $1)
             ORDER BY rank DESC
             LIMIT $2"
        )
        .bind(query)
        .bind(limit)
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
pub async fn grep_search(
    pool: &PgPool,
    pattern: &str,
    glob: Option<&str>,
    limit: i32,
) -> Result<Vec<GrepResult>, sqlx::Error> {
    let results = if let Some(glob_pattern) = glob {
        // Convert glob to SQL LIKE pattern
        let like_pattern = glob_pattern.replace('*', "%").replace('?', "_");
        sqlx::query_as::<_, GrepResult>(
            "SELECT path, relative_path, language, content
             FROM indexed_files
             WHERE content ~ $1
               AND relative_path LIKE $3
             LIMIT $2"
        )
        .bind(pattern)
        .bind(limit)
        .bind(&like_pattern)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query_as::<_, GrepResult>(
            "SELECT path, relative_path, language, content
             FROM indexed_files
             WHERE content ~ $1
             LIMIT $2"
        )
        .bind(pattern)
        .bind(limit)
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

/// Read a single file's content by path.
pub async fn read_file(pool: &PgPool, path: &str) -> Result<Option<FileContent>, sqlx::Error> {
    let row = sqlx::query_as::<_, FileContent>(
        "SELECT path, relative_path, language, content, size_bytes, line_count, truncated
         FROM indexed_files WHERE path = $1"
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
}

/// Read a single file's content by relative path.
pub async fn read_file_by_relative_path(
    pool: &PgPool,
    relative_path: &str,
) -> Result<Option<FileContent>, sqlx::Error> {
    let row = sqlx::query_as::<_, FileContent>(
        "SELECT path, relative_path, language, content, size_bytes, line_count, truncated
         FROM indexed_files WHERE relative_path = $1"
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
         ORDER BY f.relative_path"
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
    let total = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT SUM(size_bytes) FROM indexed_files"
    )
    .fetch_one(pool)
    .await?;
    Ok(total.unwrap_or(0) as u64)
}

// ============================================================================
// Completion queries
// ============================================================================

/// List all distinct project names (for completions).
pub async fn list_project_names(pool: &PgPool) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT name FROM projects ORDER BY name"
    )
    .fetch_all(pool)
    .await
}

/// List all distinct languages from indexed files (for completions).
pub async fn list_languages(pool: &PgPool) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT language FROM indexed_files ORDER BY language"
    )
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

/// Clean up stale files (files that no longer exist on disk).
pub async fn cleanup_stale_files(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let paths = sqlx::query_scalar::<_, String>(
        "SELECT path FROM indexed_files"
    )
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
