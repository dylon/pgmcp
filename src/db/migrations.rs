//! Database schema migrations.

use sqlx::PgPool;

/// Run all migrations to set up the schema.
pub async fn run_migrations(pool: &PgPool) -> Result<(), sqlx::Error> {
    // Create extensions
    sqlx::query("CREATE EXTENSION IF NOT EXISTS vector")
        .execute(pool)
        .await?;
    sqlx::query("CREATE EXTENSION IF NOT EXISTS pg_trgm")
        .execute(pool)
        .await?;

    // Create projects table
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS projects (
            id SERIAL PRIMARY KEY,
            workspace_path TEXT NOT NULL,
            path TEXT UNIQUE NOT NULL,
            name TEXT NOT NULL,
            discovered_at TIMESTAMPTZ DEFAULT NOW(),
            last_scanned_at TIMESTAMPTZ
        )"
    )
    .execute(pool)
    .await?;

    // Create indexed_files table
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS indexed_files (
            id BIGSERIAL PRIMARY KEY,
            project_id INTEGER REFERENCES projects(id) ON DELETE CASCADE,
            path TEXT UNIQUE NOT NULL,
            relative_path TEXT NOT NULL,
            language TEXT NOT NULL,
            size_bytes BIGINT NOT NULL,
            content TEXT,
            content_hash BIGINT NOT NULL,
            line_count INTEGER NOT NULL,
            truncated BOOLEAN NOT NULL DEFAULT FALSE,
            indexed_at TIMESTAMPTZ DEFAULT NOW(),
            modified_at TIMESTAMPTZ NOT NULL
        )"
    )
    .execute(pool)
    .await?;

    // Create file_chunks table
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS file_chunks (
            id BIGSERIAL PRIMARY KEY,
            file_id BIGINT REFERENCES indexed_files(id) ON DELETE CASCADE,
            chunk_index INTEGER NOT NULL,
            content TEXT NOT NULL,
            start_line INTEGER NOT NULL,
            end_line INTEGER NOT NULL,
            embedding vector(384) NOT NULL,
            UNIQUE (file_id, chunk_index)
        )"
    )
    .execute(pool)
    .await?;

    // Migration: drop the old UNIQUE composite index on projects(workspace_path, path)
    // if it exists. The path column is already UNIQUE on its own, so the composite
    // index only needs to be a regular (non-unique) index for query performance.
    // Without this, concurrent upserts hit the composite UNIQUE constraint which
    // isn't covered by ON CONFLICT (path).
    let _ = sqlx::query("DROP INDEX IF EXISTS idx_projects_workspace_path")
        .execute(pool)
        .await;

    // Create indexes (IF NOT EXISTS for idempotency)
    let indexes = [
        "CREATE INDEX IF NOT EXISTS idx_files_fts ON indexed_files USING gin(to_tsvector('english', content))",
        "CREATE INDEX IF NOT EXISTS idx_files_path_trgm ON indexed_files USING gin(relative_path gin_trgm_ops)",
        "CREATE INDEX IF NOT EXISTS idx_files_content_hash ON indexed_files(content_hash)",
        "CREATE INDEX IF NOT EXISTS idx_files_project ON indexed_files(project_id)",
        "CREATE INDEX IF NOT EXISTS idx_files_language ON indexed_files(language)",
        "CREATE INDEX IF NOT EXISTS idx_chunks_file_id ON file_chunks(file_id)",
        "CREATE INDEX IF NOT EXISTS idx_projects_workspace_path ON projects(workspace_path, path)",
    ];

    for idx_sql in &indexes {
        sqlx::query(idx_sql).execute(pool).await?;
    }

    // HNSW index for vector similarity (create if not exists)
    // Note: pgvector HNSW indexes don't support IF NOT EXISTS before certain versions
    // We'll attempt to create and ignore the error if it already exists
    let _ = sqlx::query(
        "CREATE INDEX idx_chunks_embedding ON file_chunks USING hnsw (embedding vector_cosine_ops)
         WITH (m = 16, ef_construction = 64)"
    )
    .execute(pool)
    .await;

    Ok(())
}
