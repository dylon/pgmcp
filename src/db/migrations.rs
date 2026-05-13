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

    // Create projects table.
    //
    // `git_common_dir` and `git_root_commits` group worktrees / sibling
    // clones of the same upstream repo. See
    // `~/.claude/plans/thoroughly-examine-home-dylon-workspace-melodic-cake.md`
    // for the rationale: cross-project analytics (find_duplicates,
    // find_similar_modules, refactoring_report, similarity-scan cron)
    // would otherwise count the same code as "duplicated" between
    // worktrees on different branches. The two columns capture two
    // distinct "same repo" signals:
    //
    //   git_common_dir   — canonical absolute path of the shared `.git`
    //                      directory. All worktrees of one repo share
    //                      this. (Output of `git rev-parse
    //                      --git-common-dir`, canonicalized.)
    //   git_root_commits — sorted comma-joined list of root-commit SHAs
    //                      (`git rev-list --max-parents=0 HEAD`).
    //                      Independent clones of the same upstream share
    //                      this even though their `.git` directories
    //                      are unrelated.
    //
    // Two projects are "same repo" if either column matches non-NULL.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS projects (
            id SERIAL PRIMARY KEY,
            workspace_path TEXT NOT NULL,
            path TEXT UNIQUE NOT NULL,
            name TEXT NOT NULL,
            git_common_dir TEXT,
            git_root_commits TEXT,
            discovered_at TIMESTAMPTZ DEFAULT NOW(),
            last_scanned_at TIMESTAMPTZ
        )",
    )
    .execute(pool)
    .await?;

    // Migration: add worktree-grouping columns to existing installs.
    // Idempotent — no-op when columns already present (e.g. fresh install
    // via the CREATE TABLE above).
    let _ = sqlx::query("ALTER TABLE projects ADD COLUMN IF NOT EXISTS git_common_dir TEXT")
        .execute(pool)
        .await;
    let _ = sqlx::query("ALTER TABLE projects ADD COLUMN IF NOT EXISTS git_root_commits TEXT")
        .execute(pool)
        .await;

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

    // Migration: content-based dedup + rename detection.
    //
    // `duplicate_of_file_id` (NULL = canonical row; non-NULL = duplicate
    // pointer). Duplicate rows have `content_hash` set but no
    // `file_chunks` rows of their own — chunk-bearing queries follow the
    // pointer via `COALESCE(duplicate_of_file_id, id)`.
    //
    // `ON DELETE SET NULL` so deleting a canonical leaves orphan
    // duplicates that can be promoted on the next scan (see
    // `delete_file_with_promotion` in queries.rs).
    let _ = sqlx::query(
        "ALTER TABLE indexed_files
         ADD COLUMN IF NOT EXISTS duplicate_of_file_id BIGINT
         REFERENCES indexed_files(id) ON DELETE SET NULL",
    )
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
        "CREATE INDEX IF NOT EXISTS idx_files_duplicate_of ON indexed_files(duplicate_of_file_id) WHERE duplicate_of_file_id IS NOT NULL",
        "CREATE INDEX IF NOT EXISTS idx_files_canonical_hash ON indexed_files(project_id, content_hash) WHERE duplicate_of_file_id IS NULL AND content_hash IS NOT NULL",
        "CREATE INDEX IF NOT EXISTS idx_projects_workspace_path ON projects(workspace_path, path)",
        // Partial indexes: most projects in pgmcp deployments are git
        // repos, but synthetic / vendored projects leave both columns
        // NULL — those don't need to pay storage for these indexes.
        // Same-repo lookups (e.g. NOT EXISTS … pa.git_common_dir =
        // pb.git_common_dir) hit the partial index when the column is
        // non-NULL, which is the only case where a match is possible.
        "CREATE INDEX IF NOT EXISTS idx_projects_git_common_dir ON projects(git_common_dir) WHERE git_common_dir IS NOT NULL",
        "CREATE INDEX IF NOT EXISTS idx_projects_git_root_commits ON projects(git_root_commits) WHERE git_root_commits IS NOT NULL",
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

    // Tier 0e — Tree-sitter symbol tables.
    //
    // `file_symbols` stores per-file symbol definitions extracted by the
    // tree-sitter pass: function/struct/enum/trait/interface/class/const/module
    // declarations with their byte range mapped to start_line / end_line.
    // Used by `naming_consistency`, `boilerplate_clusters` (for tree-sitter
    // identifier normalization), `extraction_candidates` (for exact call-site
    // counts), and the future symbol-aware import resolution.
    //
    // `symbol_references` stores per-call/per-type-use edges: source_line +
    // resolved target (when known) or raw target form (when unresolved).
    // The `target_symbol_id IS NULL OR target_file_id IS NULL` rows are the
    // unresolved-target equivalent of `code_graph_edges` for fine-grained
    // dep-health analysis.
    //
    // Both tables CASCADE off `indexed_files.id` — if a file is reindexed
    // (file_chunks rebuilt), its symbols and references are rebuilt too.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS file_symbols (
            id BIGSERIAL PRIMARY KEY,
            file_id BIGINT NOT NULL REFERENCES indexed_files(id) ON DELETE CASCADE,
            name TEXT NOT NULL,
            kind TEXT NOT NULL,
            start_line INTEGER NOT NULL,
            end_line INTEGER NOT NULL,
            parent_id BIGINT REFERENCES file_symbols(id) ON DELETE CASCADE,
            visibility TEXT,
            signature TEXT,
            UNIQUE (file_id, kind, name, start_line)
        )",
    )
    .execute(pool)
    .await?;

    let file_symbols_indexes = vec![
        "CREATE INDEX IF NOT EXISTS idx_file_symbols_file ON file_symbols(file_id)",
        "CREATE INDEX IF NOT EXISTS idx_file_symbols_name ON file_symbols(name)",
        "CREATE INDEX IF NOT EXISTS idx_file_symbols_kind_name ON file_symbols(kind, name)",
        "CREATE INDEX IF NOT EXISTS idx_file_symbols_name_trgm ON file_symbols USING gin (name gin_trgm_ops)",
    ];
    for idx_sql in &file_symbols_indexes {
        sqlx::query(idx_sql).execute(pool).await?;
    }

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS symbol_references (
            id BIGSERIAL PRIMARY KEY,
            source_file_id BIGINT NOT NULL REFERENCES indexed_files(id) ON DELETE CASCADE,
            source_symbol_id BIGINT REFERENCES file_symbols(id) ON DELETE SET NULL,
            target_file_id BIGINT REFERENCES indexed_files(id) ON DELETE SET NULL,
            target_symbol_id BIGINT REFERENCES file_symbols(id) ON DELETE SET NULL,
            target_raw TEXT NOT NULL,
            ref_kind TEXT NOT NULL,
            source_line INTEGER NOT NULL,
            UNIQUE (source_file_id, source_line, target_raw, ref_kind)
        )",
    )
    .execute(pool)
    .await?;

    let symbol_refs_indexes = vec![
        "CREATE INDEX IF NOT EXISTS idx_symbol_refs_source_file ON symbol_references(source_file_id)",
        "CREATE INDEX IF NOT EXISTS idx_symbol_refs_target_symbol ON symbol_references(target_symbol_id)",
        "CREATE INDEX IF NOT EXISTS idx_symbol_refs_target_raw ON symbol_references(target_raw)",
    ];
    for idx_sql in &symbol_refs_indexes {
        sqlx::query(idx_sql).execute(pool).await?;
    }

    // ================================================================
    // Software pattern / anti-pattern knowledge index
    // ================================================================

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS programming_paradigms (
            id SERIAL PRIMARY KEY,
            slug TEXT UNIQUE NOT NULL,
            name TEXT NOT NULL,
            description TEXT NOT NULL,
            wikipedia_url TEXT,
            created_at TIMESTAMPTZ DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS software_patterns (
            id BIGSERIAL PRIMARY KEY,
            slug TEXT UNIQUE NOT NULL,
            name TEXT NOT NULL,
            kind TEXT NOT NULL CHECK (kind IN ('pattern', 'anti_pattern')),
            category TEXT NOT NULL,
            summary TEXT NOT NULL,
            intent TEXT NOT NULL,
            problem TEXT NOT NULL,
            solution TEXT NOT NULL,
            consequences TEXT NOT NULL,
            tags TEXT[] NOT NULL DEFAULT ARRAY[]::TEXT[],
            canonical_url TEXT,
            created_at TIMESTAMPTZ DEFAULT NOW(),
            updated_at TIMESTAMPTZ DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS software_pattern_paradigms (
            pattern_id BIGINT REFERENCES software_patterns(id) ON DELETE CASCADE,
            paradigm_id INTEGER REFERENCES programming_paradigms(id) ON DELETE CASCADE,
            relevance DOUBLE PRECISION NOT NULL DEFAULT 1.0,
            PRIMARY KEY (pattern_id, paradigm_id)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS software_pattern_sources (
            id BIGSERIAL PRIMARY KEY,
            source_family TEXT NOT NULL,
            title TEXT NOT NULL,
            url TEXT,
            license_label TEXT,
            source_type TEXT NOT NULL,
            ingest_policy TEXT NOT NULL,
            content TEXT,
            content_hash BIGINT,
            fetched_at TIMESTAMPTZ,
            imported_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            status TEXT NOT NULL DEFAULT 'pending',
            error TEXT,
            metadata JSONB NOT NULL DEFAULT '{}'::jsonb
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS software_pattern_source_patterns (
            source_id BIGINT REFERENCES software_pattern_sources(id) ON DELETE CASCADE,
            pattern_id BIGINT REFERENCES software_patterns(id) ON DELETE CASCADE,
            relation TEXT NOT NULL DEFAULT 'documents',
            PRIMARY KEY (source_id, pattern_id)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS software_pattern_chunks (
            id BIGSERIAL PRIMARY KEY,
            source_id BIGINT REFERENCES software_pattern_sources(id) ON DELETE CASCADE,
            chunk_index INTEGER NOT NULL,
            content TEXT NOT NULL,
            start_line INTEGER NOT NULL,
            end_line INTEGER NOT NULL,
            embedding vector(384) NOT NULL,
            UNIQUE (source_id, chunk_index)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS software_pattern_import_runs (
            id BIGSERIAL PRIMARY KEY,
            mode TEXT NOT NULL,
            source_family TEXT,
            started_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            finished_at TIMESTAMPTZ,
            status TEXT NOT NULL,
            sources_seen INTEGER NOT NULL DEFAULT 0,
            sources_imported INTEGER NOT NULL DEFAULT 0,
            chunks_embedded INTEGER NOT NULL DEFAULT 0,
            error TEXT
        )",
    )
    .execute(pool)
    .await?;

    let pattern_indexes = [
        "CREATE INDEX IF NOT EXISTS idx_programming_paradigms_slug ON programming_paradigms(slug)",
        "CREATE INDEX IF NOT EXISTS idx_software_patterns_kind ON software_patterns(kind)",
        "CREATE INDEX IF NOT EXISTS idx_software_patterns_category ON software_patterns(category)",
        "CREATE INDEX IF NOT EXISTS idx_software_patterns_tags ON software_patterns USING gin(tags)",
        "CREATE INDEX IF NOT EXISTS idx_spp_paradigm ON software_pattern_paradigms(paradigm_id)",
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_sps_identity ON software_pattern_sources(source_family, title, (COALESCE(url, '')))",
        "CREATE INDEX IF NOT EXISTS idx_sps_family ON software_pattern_sources(source_family)",
        "CREATE INDEX IF NOT EXISTS idx_sps_status ON software_pattern_sources(status)",
        "CREATE INDEX IF NOT EXISTS idx_spsp_pattern ON software_pattern_source_patterns(pattern_id)",
        "CREATE INDEX IF NOT EXISTS idx_spc_source ON software_pattern_chunks(source_id)",
        "CREATE INDEX IF NOT EXISTS idx_spir_started ON software_pattern_import_runs(started_at DESC)",
    ];

    for idx_sql in &pattern_indexes {
        sqlx::query(idx_sql).execute(pool).await?;
    }

    // The original oneTBB registry entry pointed at an Intel-hosted page that
    // returned 403 to simple HTTP clients. Drop only the failed empty legacy row;
    // successful/manual imports are left intact.
    sqlx::query(
        "DELETE FROM software_pattern_sources s
         WHERE s.source_family = 'intel_onetbb'
           AND s.url = 'https://www.intel.com/content/www/us/en/docs/onetbb/developer-guide-api-reference/2022-0/design-patterns.html'
           AND s.status = 'failed'
           AND s.content IS NULL
           AND NOT EXISTS (
               SELECT 1 FROM software_pattern_chunks c WHERE c.source_id = s.id
           )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "ALTER TABLE software_patterns DROP CONSTRAINT IF EXISTS software_patterns_kind_check",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "ALTER TABLE software_patterns
            ADD CONSTRAINT software_patterns_kind_check
            CHECK (kind IN ('pattern', 'anti_pattern', 'principle', 'code_smell'))",
    )
    .execute(pool)
    .await?;

    ensure_software_pattern_hnsw_index(pool, vector_config).await?;

    // ================================================================
    // Session-level mandate observation (session_id keyed)
    // ================================================================

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS sessions (
            id          UUID PRIMARY KEY,
            cwd         TEXT NOT NULL,
            project_id  INTEGER REFERENCES projects(id) ON DELETE SET NULL,
            first_seen  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            last_seen   TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS session_prompts (
            id            BIGSERIAL PRIMARY KEY,
            session_id    UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            ts            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            prompt_text   TEXT NOT NULL,
            prompt_sha256 CHAR(64) NOT NULL,
            embedding     vector(384),
            UNIQUE (session_id, prompt_sha256)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS session_mandates (
            id                   BIGSERIAL PRIMARY KEY,
            session_id           UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            source_prompt_id     BIGINT NOT NULL REFERENCES session_prompts(id) ON DELETE CASCADE,
            polarity             TEXT NOT NULL,
            imperative           TEXT NOT NULL,
            target               TEXT,
            cwd_prefix           TEXT,
            cue_tier             CHAR(1) NOT NULL DEFAULT 'D',
            salience             REAL NOT NULL DEFAULT 1.0,
            status               TEXT NOT NULL DEFAULT 'active',
            created_at           TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            last_reinforced_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            reinforcement_count  INTEGER NOT NULL DEFAULT 1
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS durable_mandates (
            id                  BIGSERIAL PRIMARY KEY,
            scope               TEXT NOT NULL,
            project_id          INTEGER REFERENCES projects(id) ON DELETE CASCADE,
            polarity            TEXT NOT NULL,
            imperative          TEXT NOT NULL,
            target              TEXT,
            source_mandate_id   BIGINT REFERENCES session_mandates(id) ON DELETE SET NULL,
            promoted_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            file_path           TEXT
        )",
    )
    .execute(pool)
    .await?;

    let session_indexes = [
        "CREATE INDEX IF NOT EXISTS idx_sessions_last_seen ON sessions(last_seen DESC)",
        "CREATE INDEX IF NOT EXISTS idx_sessions_cwd       ON sessions(cwd)",
        "CREATE INDEX IF NOT EXISTS idx_session_prompts_session_ts ON session_prompts(session_id, ts DESC)",
        "CREATE INDEX IF NOT EXISTS idx_session_mandates_session_status ON session_mandates(session_id, status)",
        "CREATE INDEX IF NOT EXISTS idx_session_mandates_cwd ON session_mandates(cwd_prefix)",
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_session_mandates_unique ON session_mandates(session_id, polarity, lower(imperative))",
        "CREATE INDEX IF NOT EXISTS idx_durable_mandates_scope_project ON durable_mandates(scope, project_id)",
    ];
    for idx_sql in &session_indexes {
        sqlx::query(idx_sql).execute(pool).await?;
    }

    // Idempotent CHECK-constraint installs (DROP IF EXISTS + ADD).
    sqlx::query(
        "ALTER TABLE session_mandates DROP CONSTRAINT IF EXISTS session_mandates_polarity_check",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_mandates
            ADD CONSTRAINT session_mandates_polarity_check
            CHECK (polarity IN ('always','never','prefer','avoid','remember','from_now_on',
                                'correction','permission','constraint','mandate',
                                'process_rule','project_rule'))",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_mandates DROP CONSTRAINT IF EXISTS session_mandates_status_check",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "ALTER TABLE session_mandates
            ADD CONSTRAINT session_mandates_status_check
            CHECK (status IN ('active','superseded','retired','promoted'))",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "ALTER TABLE durable_mandates DROP CONSTRAINT IF EXISTS durable_mandates_scope_check",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "ALTER TABLE durable_mandates
            ADD CONSTRAINT durable_mandates_scope_check
            CHECK (scope IN ('project','workspace'))",
    )
    .execute(pool)
    .await?;

    ensure_session_prompts_hnsw_index(pool, vector_config).await?;

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

/// Ensure HNSW index on software-pattern knowledge chunk embeddings.
async fn ensure_software_pattern_hnsw_index(
    pool: &PgPool,
    config: &VectorConfig,
) -> Result<(), sqlx::Error> {
    let current_params = format!(
        "m={},ef_construction={}",
        config.hnsw_m, config.hnsw_ef_construction
    );

    let stored: Option<String> = sqlx::query_scalar::<_, String>(
        "SELECT value FROM pgmcp_metadata WHERE key = 'software_pattern_hnsw_params'",
    )
    .fetch_optional(pool)
    .await?;

    let needs_rebuild = stored.as_deref() != Some(&current_params);

    if needs_rebuild {
        let _ = sqlx::query("DROP INDEX IF EXISTS idx_software_pattern_chunks_embedding")
            .execute(pool)
            .await;

        let create_sql = format!(
            "CREATE INDEX idx_software_pattern_chunks_embedding ON software_pattern_chunks \
             USING hnsw (embedding vector_cosine_ops) WITH (m = {}, ef_construction = {})",
            config.hnsw_m, config.hnsw_ef_construction
        );
        let _ = sqlx::query(&create_sql).execute(pool).await;

        sqlx::query(
            "INSERT INTO pgmcp_metadata (key, value)
             VALUES ('software_pattern_hnsw_params', $1)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(&current_params)
        .execute(pool)
        .await?;

        tracing::info!("Software pattern chunks HNSW index created/rebuilt");
    }

    Ok(())
}

/// HNSW index for `session_prompts.embedding`. Mirrors the software-pattern
/// helper above. Rebuilt only when `[vector]` params change.
async fn ensure_session_prompts_hnsw_index(
    pool: &PgPool,
    config: &VectorConfig,
) -> Result<(), sqlx::Error> {
    let current_params = format!(
        "m={},ef_construction={}",
        config.hnsw_m, config.hnsw_ef_construction
    );

    let stored: Option<String> = sqlx::query_scalar::<_, String>(
        "SELECT value FROM pgmcp_metadata WHERE key = 'session_prompts_hnsw_params'",
    )
    .fetch_optional(pool)
    .await?;

    if stored.as_deref() != Some(&current_params) {
        let _ = sqlx::query("DROP INDEX IF EXISTS idx_session_prompts_embedding")
            .execute(pool)
            .await;

        let create_sql = format!(
            "CREATE INDEX idx_session_prompts_embedding ON session_prompts \
             USING hnsw (embedding vector_cosine_ops) WITH (m = {}, ef_construction = {})",
            config.hnsw_m, config.hnsw_ef_construction
        );
        let _ = sqlx::query(&create_sql).execute(pool).await;

        sqlx::query(
            "INSERT INTO pgmcp_metadata (key, value)
             VALUES ('session_prompts_hnsw_params', $1)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(&current_params)
        .execute(pool)
        .await?;

        tracing::info!("Session prompts HNSW index created/rebuilt");
    }

    Ok(())
}
