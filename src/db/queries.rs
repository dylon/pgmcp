//! Database query functions.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

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
         RETURNING id",
    )
    .bind(workspace_path)
    .bind(path)
    .bind(name)
    .fetch_one(pool)
    .await?;

    Ok(row)
}

/// List all projects with file counts.
pub async fn list_projects(pool: &PgPool) -> Result<Vec<ProjectInfo>, sqlx::Error> {
    let rows = sqlx::query_as::<_, ProjectInfo>(
        "SELECT p.id, p.workspace_path, p.path, p.name, p.discovered_at, p.last_scanned_at,
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
        "SELECT p.id, p.workspace_path, p.path, p.name, p.discovered_at, p.last_scanned_at,
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
                        1 - (c.embedding <=> $1) as score,
                        p.name as project_name
                 FROM file_chunks c
                 JOIN indexed_files f ON f.id = c.file_id
                 JOIN projects p ON p.id = f.project_id
                 WHERE f.language = $3 AND p.name = $4
                 ORDER BY c.embedding <=> $1
                 LIMIT $2",
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
                        1 - (c.embedding <=> $1) as score,
                        p.name as project_name
                 FROM file_chunks c
                 JOIN indexed_files f ON f.id = c.file_id
                 JOIN projects p ON p.id = f.project_id
                 WHERE f.language = $3
                 ORDER BY c.embedding <=> $1
                 LIMIT $2",
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
                        1 - (c.embedding <=> $1) as score,
                        p.name as project_name
                 FROM file_chunks c
                 JOIN indexed_files f ON f.id = c.file_id
                 JOIN projects p ON p.id = f.project_id
                 WHERE p.name = $3
                 ORDER BY c.embedding <=> $1
                 LIMIT $2",
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
                        1 - (c.embedding <=> $1) as score,
                        p.name as project_name
                 FROM file_chunks c
                 JOIN indexed_files f ON f.id = c.file_id
                 JOIN projects p ON p.id = f.project_id
                 ORDER BY c.embedding <=> $1
                 LIMIT $2",
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
             LIMIT $2",
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
             LIMIT $2",
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
             LIMIT $2",
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
             LIMIT $2",
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
}

/// Read a single file's content by relative path.
pub async fn read_file_by_relative_path(
    pool: &PgPool,
    relative_path: &str,
) -> Result<Option<FileContent>, sqlx::Error> {
    let row = sqlx::query_as::<_, FileContent>(
        "SELECT path, relative_path, language, content, size_bytes, line_count, truncated
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

    let results = sqlx::query_as::<_, SimilarityNeighborRow>(
        "WITH batch AS (
            SELECT c.id, c.file_id, c.embedding, f.project_id, f.path, f.language, p.name as project_name
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
            ORDER BY c2.embedding <=> b.embedding
            LIMIT $3
        ) nn
        WHERE nn.similarity >= $4"
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
) -> Result<Vec<FileSimilarityPair>, sqlx::Error> {
    if let Some(proj) = target_project {
        sqlx::query_as::<_, FileSimilarityPair>(
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
             GROUP BY file_id_a, path_a, project_name_a, file_id_b, path_b, project_name_b, s.language
             ORDER BY avg_similarity DESC
             LIMIT $3"
        )
        .bind(file_id)
        .bind(min_similarity)
        .bind(limit)
        .bind(proj)
        .fetch_all(pool)
        .await
    } else {
        sqlx::query_as::<_, FileSimilarityPair>(
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
             GROUP BY file_id_a, path_a, project_name_a, file_id_b, path_b, project_name_b, s.language
             ORDER BY avg_similarity DESC
             LIMIT $3"
        )
        .bind(file_id)
        .bind(min_similarity)
        .bind(limit)
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
pub async fn find_duplicate_file_pairs(
    pool: &PgPool,
    min_similarity: f64,
    language: Option<&str>,
    limit: i32,
) -> Result<Vec<DuplicateFilePair>, sqlx::Error> {
    if let Some(lang) = language {
        sqlx::query_as::<_, DuplicateFilePair>(
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
             GROUP BY s.file_id_a, s.path_a, s.project_name_a, s.project_id_a,
                      s.file_id_b, s.path_b, s.project_name_b, s.project_id_b,
                      s.language
             HAVING AVG(s.chunk_similarity) >= $1
             ORDER BY avg_similarity DESC
             LIMIT $2",
        )
        .bind(min_similarity)
        .bind(limit)
        .bind(lang)
        .fetch_all(pool)
        .await
    } else {
        sqlx::query_as::<_, DuplicateFilePair>(
            "SELECT s.file_id_a, s.path_a, s.project_name_a, s.project_id_a,
                    s.file_id_b, s.path_b, s.project_name_b, s.project_id_b,
                    s.language,
                    AVG(s.chunk_similarity) as avg_similarity,
                    MAX(s.chunk_similarity) as max_similarity,
                    COUNT(*) as matching_chunks
             FROM cross_project_similarities s
             WHERE s.chunk_similarity >= $1
               AND s.project_id_a != s.project_id_b
             GROUP BY s.file_id_a, s.path_a, s.project_name_a, s.project_id_a,
                      s.file_id_b, s.path_b, s.project_name_b, s.project_id_b,
                      s.language
             HAVING AVG(s.chunk_similarity) >= $1
             ORDER BY avg_similarity DESC
             LIMIT $2",
        )
        .bind(min_similarity)
        .bind(limit)
        .fetch_all(pool)
        .await
    }
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
pub async fn bulk_extract_embeddings(
    pool: &PgPool,
    language: Option<&str>,
) -> Result<Vec<ChunkEmbeddingRow>, sqlx::Error> {
    if let Some(lang) = language {
        let rows = sqlx::query_as::<_, BulkChunkRow>(
            "SELECT c.id as chunk_id, c.file_id, f.project_id, p.name as project_name,
                    f.path, f.language, c.content, c.embedding::real[] as embedding
             FROM file_chunks c
             JOIN indexed_files f ON f.id = c.file_id
             JOIN projects p ON p.id = f.project_id
             WHERE f.language = $1
             ORDER BY c.id",
        )
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
             ORDER BY c.id",
        )
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

        let topic_id = sqlx::query_scalar::<_, i32>(
            "INSERT INTO code_topics
                (scope, cluster_index, label, chunk_count, file_count, project_count,
                 project_names, avg_internal_similarity, representative_chunk_id,
                 representative_snippet, top_files, keywords, keyword_scores,
                 centroid, parent_topic_ids)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
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
        .fetch_one(pool)
        .await?;

        // Store chunk-to-topic assignments with membership degrees
        for (idx, &chunk_id) in topic.chunk_ids.iter().enumerate() {
            let membership = topic.memberships.get(idx).copied().unwrap_or(1.0);
            sqlx::query(
                "INSERT INTO chunk_topic_assignments (chunk_id, topic_id, membership_score)
                 VALUES ($1, $2, $3)
                 ON CONFLICT (chunk_id, topic_id) DO UPDATE SET
                    membership_score = EXCLUDED.membership_score",
            )
            .bind(chunk_id)
            .bind(topic_id)
            .bind(membership)
            .execute(pool)
            .await?;
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
