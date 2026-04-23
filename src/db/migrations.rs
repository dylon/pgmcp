//! Database schema migrations.

use sqlx::PgPool;

use crate::config::VectorConfig;

/// Run all migrations to set up the schema.
pub async fn run_migrations(
    pool: &PgPool,
    vector_config: &VectorConfig,
) -> Result<(), sqlx::Error> {
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
        )",
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
            content_hash BIGINT,
            line_count INTEGER NOT NULL,
            truncated BOOLEAN NOT NULL DEFAULT FALSE,
            indexed_at TIMESTAMPTZ DEFAULT NOW(),
            modified_at TIMESTAMPTZ NOT NULL
        )",
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
        )",
    )
    .execute(pool)
    .await?;

    // Migration: allow content_hash to be NULL (deferred commit for resume-safety).
    // Existing databases may have NOT NULL; this is metadata-only in PostgreSQL.
    let _ = sqlx::query("ALTER TABLE indexed_files ALTER COLUMN content_hash DROP NOT NULL")
        .execute(pool)
        .await;

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

    // HNSW index for vector similarity.
    // Drop and recreate if the index params have changed (m, ef_construction).
    ensure_hnsw_index(pool, vector_config).await?;

    // ================================================================
    // Git history tables
    // ================================================================

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS git_commits (
            id BIGSERIAL PRIMARY KEY,
            project_id INTEGER REFERENCES projects(id) ON DELETE CASCADE,
            commit_hash TEXT NOT NULL,
            author TEXT NOT NULL,
            author_date TIMESTAMPTZ NOT NULL,
            subject TEXT NOT NULL,
            body TEXT,
            UNIQUE (project_id, commit_hash)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS git_commit_chunks (
            id BIGSERIAL PRIMARY KEY,
            commit_id BIGINT REFERENCES git_commits(id) ON DELETE CASCADE,
            chunk_index INTEGER NOT NULL,
            content TEXT NOT NULL,
            embedding vector(384) NOT NULL,
            UNIQUE (commit_id, chunk_index)
        )",
    )
    .execute(pool)
    .await?;

    // Blame metadata on file_chunks (idempotent ALTER)
    let _ = sqlx::query("ALTER TABLE file_chunks ADD COLUMN IF NOT EXISTS blame_commit TEXT")
        .execute(pool)
        .await;
    let _ = sqlx::query("ALTER TABLE file_chunks ADD COLUMN IF NOT EXISTS blame_author TEXT")
        .execute(pool)
        .await;
    let _ = sqlx::query("ALTER TABLE file_chunks ADD COLUMN IF NOT EXISTS blame_date TIMESTAMPTZ")
        .execute(pool)
        .await;

    // Indexes for git tables
    let git_indexes = [
        "CREATE INDEX IF NOT EXISTS idx_git_commits_project ON git_commits(project_id)",
        "CREATE INDEX IF NOT EXISTS idx_git_commits_hash ON git_commits(commit_hash)",
        "CREATE INDEX IF NOT EXISTS idx_git_commit_chunks_commit ON git_commit_chunks(commit_id)",
    ];

    for idx_sql in &git_indexes {
        sqlx::query(idx_sql).execute(pool).await?;
    }

    // HNSW index for git commit chunk embeddings
    ensure_git_commit_hnsw_index(pool, vector_config).await?;

    // ================================================================
    // Cross-project similarity analysis table
    // ================================================================

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS cross_project_similarities (
            id BIGSERIAL PRIMARY KEY,
            chunk_id_a BIGINT REFERENCES file_chunks(id) ON DELETE CASCADE,
            file_id_a BIGINT REFERENCES indexed_files(id) ON DELETE CASCADE,
            project_id_a INTEGER REFERENCES projects(id) ON DELETE CASCADE,
            chunk_id_b BIGINT REFERENCES file_chunks(id) ON DELETE CASCADE,
            file_id_b BIGINT REFERENCES indexed_files(id) ON DELETE CASCADE,
            project_id_b INTEGER REFERENCES projects(id) ON DELETE CASCADE,
            chunk_similarity DOUBLE PRECISION NOT NULL,
            path_a TEXT NOT NULL,
            path_b TEXT NOT NULL,
            project_name_a TEXT NOT NULL,
            project_name_b TEXT NOT NULL,
            language TEXT NOT NULL,
            computed_at TIMESTAMPTZ DEFAULT NOW(),
            CONSTRAINT pair_ordering CHECK (chunk_id_a < chunk_id_b)
        )",
    )
    .execute(pool)
    .await?;

    let similarity_indexes = [
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_similarity_pair ON cross_project_similarities(chunk_id_a, chunk_id_b)",
        "CREATE INDEX IF NOT EXISTS idx_similarity_project_a ON cross_project_similarities(project_id_a)",
        "CREATE INDEX IF NOT EXISTS idx_similarity_project_b ON cross_project_similarities(project_id_b)",
        "CREATE INDEX IF NOT EXISTS idx_similarity_score ON cross_project_similarities(chunk_similarity DESC)",
        "CREATE INDEX IF NOT EXISTS idx_similarity_file_a ON cross_project_similarities(file_id_a)",
        "CREATE INDEX IF NOT EXISTS idx_similarity_file_b ON cross_project_similarities(file_id_b)",
    ];

    for idx_sql in &similarity_indexes {
        sqlx::query(idx_sql).execute(pool).await?;
    }

    // ================================================================
    // Code topic clustering tables
    // ================================================================

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS code_topics (
            id SERIAL PRIMARY KEY,
            scope TEXT NOT NULL,
            cluster_index INTEGER NOT NULL,
            label TEXT NOT NULL,
            chunk_count INTEGER NOT NULL,
            file_count INTEGER NOT NULL,
            project_count INTEGER NOT NULL,
            project_names TEXT[] NOT NULL,
            avg_internal_similarity DOUBLE PRECISION,
            representative_chunk_id BIGINT REFERENCES file_chunks(id) ON DELETE SET NULL,
            representative_snippet TEXT,
            top_files JSONB,
            computed_at TIMESTAMPTZ DEFAULT NOW(),
            UNIQUE(scope, cluster_index)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_code_topics_scope ON code_topics(scope)")
        .execute(pool)
        .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS chunk_topic_assignments (
            chunk_id BIGINT REFERENCES file_chunks(id) ON DELETE CASCADE,
            topic_id INTEGER REFERENCES code_topics(id) ON DELETE CASCADE,
            membership_score DOUBLE PRECISION NOT NULL DEFAULT 1.0,
            PRIMARY KEY (chunk_id, topic_id)
        )",
    )
    .execute(pool)
    .await?;

    // Migration: add keywords/keyword_scores to code_topics (idempotent)
    let _ = sqlx::query("ALTER TABLE code_topics ADD COLUMN IF NOT EXISTS keywords TEXT[]")
        .execute(pool)
        .await;
    let _ = sqlx::query("ALTER TABLE code_topics ADD COLUMN IF NOT EXISTS keyword_scores REAL[]")
        .execute(pool)
        .await;

    // Phase 7: store centroid vector for FCM warm-start across restarts.
    let _ = sqlx::query("ALTER TABLE code_topics ADD COLUMN IF NOT EXISTS centroid REAL[]")
        .execute(pool)
        .await;

    // Phase 9: meta-cluster hierarchy stores parent_topic_ids on scope='hierarchy' rows.
    let _ =
        sqlx::query("ALTER TABLE code_topics ADD COLUMN IF NOT EXISTS parent_topic_ids BIGINT[]")
            .execute(pool)
            .await;

    let topic_indexes = [
        "CREATE INDEX IF NOT EXISTS idx_cta_topic ON chunk_topic_assignments(topic_id)",
        "CREATE INDEX IF NOT EXISTS idx_cta_chunk ON chunk_topic_assignments(chunk_id)",
    ];

    for idx_sql in &topic_indexes {
        sqlx::query(idx_sql).execute(pool).await?;
    }

    // ================================================================
    // Git commit files table (for co-change coupling analysis)
    // ================================================================

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS git_commit_files (
            id BIGSERIAL PRIMARY KEY,
            commit_id BIGINT NOT NULL REFERENCES git_commits(id) ON DELETE CASCADE,
            file_path TEXT NOT NULL,
            change_type CHAR(1) NOT NULL,
            UNIQUE(commit_id, file_path)
        )",
    )
    .execute(pool)
    .await?;

    let gcf_indexes = [
        "CREATE INDEX IF NOT EXISTS idx_gcf_commit ON git_commit_files(commit_id)",
        "CREATE INDEX IF NOT EXISTS idx_gcf_path ON git_commit_files(file_path)",
    ];

    for idx_sql in &gcf_indexes {
        sqlx::query(idx_sql).execute(pool).await?;
    }

    // ================================================================
    // Code graph edges table (import/dependency relationships)
    // ================================================================

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS code_graph_edges (
            id BIGSERIAL PRIMARY KEY,
            project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            source_file_id BIGINT NOT NULL REFERENCES indexed_files(id) ON DELETE CASCADE,
            target_file_id BIGINT REFERENCES indexed_files(id) ON DELETE SET NULL,
            edge_type TEXT NOT NULL,
            target_raw TEXT,
            weight DOUBLE PRECISION DEFAULT 1.0,
            computed_at TIMESTAMPTZ DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await?;

    let graph_edge_indexes = [
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_cge_unique ON code_graph_edges(source_file_id, COALESCE(target_file_id, -1::BIGINT), edge_type, COALESCE(target_raw, ''))",
        "CREATE INDEX IF NOT EXISTS idx_cge_source ON code_graph_edges(source_file_id)",
        "CREATE INDEX IF NOT EXISTS idx_cge_target ON code_graph_edges(target_file_id)",
        "CREATE INDEX IF NOT EXISTS idx_cge_project_type ON code_graph_edges(project_id, edge_type)",
    ];

    for idx_sql in &graph_edge_indexes {
        sqlx::query(idx_sql).execute(pool).await?;
    }

    // ================================================================
    // File metrics table (precomputed per-file graph & quality metrics)
    // ================================================================

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS file_metrics (
            file_id BIGINT PRIMARY KEY REFERENCES indexed_files(id) ON DELETE CASCADE,
            project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            pagerank DOUBLE PRECISION,
            betweenness DOUBLE PRECISION,
            in_degree INTEGER DEFAULT 0,
            out_degree INTEGER DEFAULT 0,
            afferent_coupling INTEGER DEFAULT 0,
            efferent_coupling INTEGER DEFAULT 0,
            instability DOUBLE PRECISION,
            commit_count INTEGER DEFAULT 0,
            author_count INTEGER DEFAULT 0,
            fix_commit_ratio DOUBLE PRECISION DEFAULT 0.0,
            churn_rate DOUBLE PRECISION DEFAULT 0.0,
            days_since_last_change INTEGER,
            bug_proneness DOUBLE PRECISION,
            tech_debt_score DOUBLE PRECISION,
            health_score DOUBLE PRECISION,
            computed_at TIMESTAMPTZ DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await?;

    let file_metrics_indexes =
        ["CREATE INDEX IF NOT EXISTS idx_fm_project ON file_metrics(project_id)"];

    for idx_sql in &file_metrics_indexes {
        sqlx::query(idx_sql).execute(pool).await?;
    }

    Ok(())
}

/// Ensure the HNSW index exists with the configured parameters.
/// If the index exists with different params, drop and recreate it.
/// Uses a metadata table to track which params the current index was built with.
async fn ensure_hnsw_index(pool: &PgPool, config: &VectorConfig) -> Result<(), sqlx::Error> {
    // Create metadata table for tracking index parameters
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS pgmcp_metadata (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        )",
    )
    .execute(pool)
    .await?;

    let current_params = format!(
        "m={},ef_construction={}",
        config.hnsw_m, config.hnsw_ef_construction
    );

    // Check if the stored params match the configured ones
    let stored: Option<String> = sqlx::query_scalar::<_, String>(
        "SELECT value FROM pgmcp_metadata WHERE key = 'hnsw_params'",
    )
    .fetch_optional(pool)
    .await?;

    let needs_rebuild = stored.as_deref() != Some(&current_params);

    if needs_rebuild {
        // Drop old index if it exists
        let _ = sqlx::query("DROP INDEX IF EXISTS idx_chunks_embedding")
            .execute(pool)
            .await;

        // Create new HNSW index with configured parameters
        let create_sql = format!(
            "CREATE INDEX idx_chunks_embedding ON file_chunks USING hnsw (embedding vector_cosine_ops) \
             WITH (m = {}, ef_construction = {})",
            config.hnsw_m, config.hnsw_ef_construction
        );
        // Ignore error if table is empty (index creation on empty table is fast)
        let _ = sqlx::query(&create_sql).execute(pool).await;

        // Store the params we built the index with
        sqlx::query(
            "INSERT INTO pgmcp_metadata (key, value) VALUES ('hnsw_params', $1)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(&current_params)
        .execute(pool)
        .await?;

        tracing::info!(
            hnsw_m = config.hnsw_m,
            hnsw_ef_construction = config.hnsw_ef_construction,
            "HNSW index created/rebuilt with updated parameters"
        );
    }

    Ok(())
}

/// Ensure HNSW index on git_commit_chunks embeddings.
async fn ensure_git_commit_hnsw_index(
    pool: &PgPool,
    config: &VectorConfig,
) -> Result<(), sqlx::Error> {
    let current_params = format!(
        "m={},ef_construction={}",
        config.hnsw_m, config.hnsw_ef_construction
    );

    let stored: Option<String> = sqlx::query_scalar::<_, String>(
        "SELECT value FROM pgmcp_metadata WHERE key = 'git_hnsw_params'",
    )
    .fetch_optional(pool)
    .await?;

    let needs_rebuild = stored.as_deref() != Some(&current_params);

    if needs_rebuild {
        let _ = sqlx::query("DROP INDEX IF EXISTS idx_git_commit_chunks_embedding")
            .execute(pool)
            .await;

        let create_sql = format!(
            "CREATE INDEX idx_git_commit_chunks_embedding ON git_commit_chunks \
             USING hnsw (embedding vector_cosine_ops) WITH (m = {}, ef_construction = {})",
            config.hnsw_m, config.hnsw_ef_construction
        );
        let _ = sqlx::query(&create_sql).execute(pool).await;

        sqlx::query(
            "INSERT INTO pgmcp_metadata (key, value) VALUES ('git_hnsw_params', $1)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(&current_params)
        .execute(pool)
        .await?;

        tracing::info!("Git commit chunks HNSW index created/rebuilt");
    }

    Ok(())
}
