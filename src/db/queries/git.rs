//! Git-history queries (commit upsert/chunk insert/last-commit watermark/
//! blame/commit semantic search/commit-file tracking). Extracted from `queries.rs` (god-file split).
#![allow(unused_imports)]

use crate::db::queries::*;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

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

/// Insert a git commit chunk with its embedding. BGE-M3-only: writes to
/// the 1024-d `embedding_v2` column, stamping the `bge-m3-v1`
/// `embedding_signature`. A non-1024 dim surfaces a clear
/// configuration-error message.
pub async fn insert_git_commit_chunk(
    pool: &PgPool,
    commit_id: i64,
    chunk_index: i32,
    content: &str,
    embedding: &[f32],
) -> Result<(), sqlx::Error> {
    if embedding.len() != 1024 {
        return Err(sqlx::Error::Protocol(format!(
            "insert_git_commit_chunk: expected a 1024-dimension BGE-M3 embedding, got {}",
            embedding.len()
        )));
    }
    let embedding_vec = pgvector::Vector::from(embedding.to_vec());
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

/// Semantic search across git commit chunks. BGE-M3-only: reads from the
/// 1024-d `embedding_v2` column on `git_commit_chunks`.
pub async fn semantic_search_commits(
    pool: &PgPool,
    embedding: &[f32],
    limit: i32,
    project: Option<&str>,
    ef_search: i32,
) -> Result<Vec<CommitSearchResult>, sqlx::Error> {
    if embedding.len() != 1024 {
        return Err(sqlx::Error::Protocol(format!(
            "semantic_search_commits: expected a 1024-dimension BGE-M3 query embedding, got {}",
            embedding.len()
        )));
    }
    let embedding_vec = pgvector::Vector::from(embedding.to_vec());

    let col = "embedding_v2";

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

/// Check if git_commit_files has data for a resolved project id.
pub async fn has_commit_files_for_project_id(
    pool: &PgPool,
    project_id: i32,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
             SELECT 1
             FROM git_commit_files gcf
             JOIN git_commits gc ON gc.id = gcf.commit_id
             WHERE gc.project_id = $1
             LIMIT 1
         )",
    )
    .bind(project_id)
    .fetch_one(pool)
    .await
}
