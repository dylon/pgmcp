//! Database schema migrations.
//!
//! ## Versioning
//!
//! `pgmcp_schema_versions` records the set of numbered migration steps
//! that have completed against this database. New schema changes that
//! aren't naturally idempotent (e.g. table-level data backfills, type
//! transformations) should be added as `apply_step(pool, N,
//! "name", || async { ... })`. The pre-versioning body that builds the
//! initial schema is registered as version 1 at the end of
//! `run_migrations` — it stays inline rather than getting moved inside
//! an `apply_step` closure because every statement in it is already
//! `IF NOT EXISTS` / `IF EXISTS` idempotent and the body bundles
//! cross-cutting concerns (HNSW index rebuilds keyed off
//! `pgmcp_metadata`, conditional column adds) that don't slot cleanly
//! into a numbered-step model. The version stamp is what makes the
//! "this DB has been through `run_migrations` at least once" check
//! cheap going forward.

use sqlx::PgPool;

mod schema_introspect;
mod v2_shadow_asr;
mod v3_cross_language_signatures;
mod versioning;
use schema_introspect::*;
use versioning::*;

use tracing::info;

use crate::config::VectorConfig;

const INITIAL_SCHEMA_VERSION: i32 = 1;

/// Run a HNSW `CREATE INDEX` with HNSW-friendly session settings.
///
/// pgvector's HNSW build phase needs the graph to fit in
/// `maintenance_work_mem`; on the PG cluster default of 64 MB it
/// spills to a slow disk-merge path at ~12k tuples and blows past
/// the daemon's 30 s `statement_timeout` (`src/db/pool.rs`) on any
/// matview / table large enough to matter. This helper opens a
/// transaction, bumps memory + sets the per-session statement
/// timeout + enables parallel build workers (pgvector ≥ 0.6
/// supports parallel HNSW build), runs the CREATE INDEX, and
/// commits. All three `SET LOCAL` effects are scoped to the
/// transaction.
///
/// The three knobs (`hnsw_maintenance_work_mem`,
/// `hnsw_build_statement_timeout_secs`, `hnsw_max_parallel_workers`)
/// live on `[vector]` config — defaults are `"2GB"`, `0` (no
/// limit), and `4` respectively. See plan F8 in
/// `~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md`.
async fn build_hnsw_index(
    pool: &PgPool,
    config: &VectorConfig,
    create_index_sql: &str,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(&format!(
        "SET LOCAL maintenance_work_mem = '{}'",
        config.hnsw_maintenance_work_mem.replace('\'', "''")
    ))
    .execute(&mut *tx)
    .await?;
    sqlx::query(&format!(
        "SET LOCAL statement_timeout = {}",
        config
            .hnsw_build_statement_timeout_secs
            .saturating_mul(1000)
    ))
    .execute(&mut *tx)
    .await?;
    sqlx::query(&format!(
        "SET LOCAL max_parallel_maintenance_workers = {}",
        config.hnsw_max_parallel_workers
    ))
    .execute(&mut *tx)
    .await?;
    sqlx::query(create_index_sql).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// Run all migrations to set up the schema.
pub async fn run_migrations(
    pool: &PgPool,
    vector_config: &VectorConfig,
) -> Result<(), sqlx::Error> {
    // Bootstrap the version table first. Subsequent migration code can
    // call `version_applied` / `record_version` to short-circuit work
    // that has already been performed.
    ensure_schema_versions_table(pool).await?;
    let initial_schema_done = version_applied(pool, INITIAL_SCHEMA_VERSION).await?;
    // Create extensions
    sqlx::query("CREATE EXTENSION IF NOT EXISTS vector")
        .execute(pool)
        .await?;
    sqlx::query("CREATE EXTENSION IF NOT EXISTS pg_trgm")
        .execute(pool)
        .await?;
    // `fuzzystrmatch` is no longer requested. Phase 3 of the integration
    // plan `~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md`
    // moved the near-duplicate mandate dedupe in
    // `src/sessions.rs::mark_near_duplicate_superseded` from SQL-side
    // `levenshtein_less_equal` to an in-process
    // `liblevenshtein::Transducer` over `DynamicDawgChar`. Existing installs
    // that already have the extension keep it (no DROP EXTENSION here);
    // new installs simply no longer request it.

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
    sqlx::query("ALTER TABLE projects ADD COLUMN IF NOT EXISTS git_common_dir TEXT")
        .execute(pool)
        .await?;
    sqlx::query("ALTER TABLE projects ADD COLUMN IF NOT EXISTS git_root_commits TEXT")
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
    sqlx::query("ALTER TABLE indexed_files ALTER COLUMN content_hash DROP NOT NULL")
        .execute(pool)
        .await?;

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
    sqlx::query(
        "ALTER TABLE indexed_files
         ADD COLUMN IF NOT EXISTS duplicate_of_file_id BIGINT
         REFERENCES indexed_files(id) ON DELETE SET NULL",
    )
    .execute(pool)
    .await?;

    // Asymmetric content storage flag. `content_recoverable_from_disk =
    // true` means the row deliberately stores `content = NULL` because
    // the file lives on the local filesystem and `read_to_string(path)`
    // can recreate it byte-for-byte cheaply (after content_hash
    // verification). Set by the indexer for plain-text languages
    // (`.md`, `.rs`, `.py`, `.txt`, `.jsonl`, …); always `false` for
    // document languages whose `indexed_files.content` holds the
    // already-extracted pandoc/pdftotext output that would be expensive
    // to recreate. The flag is independent of the `truncated` flag
    // (which still signals size-gated oversize files).
    sqlx::query(
        "ALTER TABLE indexed_files
         ADD COLUMN IF NOT EXISTS content_recoverable_from_disk BOOLEAN
         NOT NULL DEFAULT FALSE",
    )
    .execute(pool)
    .await?;

    // Migration: drop the old UNIQUE composite index on projects(workspace_path, path)
    // if it exists. The path column is already UNIQUE on its own, so the composite
    // index only needs to be a regular (non-unique) index for query performance.
    // Without this, concurrent upserts hit the composite UNIQUE constraint which
    // isn't covered by ON CONFLICT (path).
    sqlx::query("DROP INDEX IF EXISTS idx_projects_workspace_path")
        .execute(pool)
        .await?;

    // Drop the legacy per-file FTS index — `text_search` now queries
    // `file_chunks.content` exclusively. The legacy index would also
    // overflow Postgres's 1 MiB tsvector limit on large `.jsonl`
    // tool-result transcripts (whose content was the cause of the
    // 2026-05-13 "string is too long for tsvector" errors before the
    // byte-aware chunker landed).
    sqlx::query("DROP INDEX IF EXISTS idx_files_fts")
        .execute(pool)
        .await?;

    // Create indexes (IF NOT EXISTS for idempotency)
    let indexes = [
        // Per-chunk FTS replaces the dropped per-file index. Chunk
        // content is bounded above by TSVECTOR_SAFE_CHUNK_BYTES (900 KiB)
        // so every chunk fits comfortably under the 1 MiB tsvector cap.
        "CREATE INDEX IF NOT EXISTS idx_file_chunks_fts ON file_chunks USING gin(to_tsvector('english', content))",
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
    sqlx::query("ALTER TABLE file_chunks ADD COLUMN IF NOT EXISTS blame_commit TEXT")
        .execute(pool)
        .await?;
    sqlx::query("ALTER TABLE file_chunks ADD COLUMN IF NOT EXISTS blame_author TEXT")
        .execute(pool)
        .await?;
    sqlx::query("ALTER TABLE file_chunks ADD COLUMN IF NOT EXISTS blame_date TIMESTAMPTZ")
        .execute(pool)
        .await?;

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
    sqlx::query("ALTER TABLE code_topics ADD COLUMN IF NOT EXISTS keywords TEXT[]")
        .execute(pool)
        .await?;
    sqlx::query("ALTER TABLE code_topics ADD COLUMN IF NOT EXISTS keyword_scores REAL[]")
        .execute(pool)
        .await?;

    // Phase 7: store centroid vector for FCM warm-start across restarts.
    sqlx::query("ALTER TABLE code_topics ADD COLUMN IF NOT EXISTS centroid REAL[]")
        .execute(pool)
        .await?;

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
    // RAPTOR-over-code summary tree (graph-roadmap Phase 3.3)
    // ----------------------------------------------------------------
    // Per project, the `code-raptor` cron clusters file-chunk embeddings
    // (CUDA FCM) and emits one level-1 summary per cluster — a conceptual
    // "module gist" that no single chunk contains. The cluster centroid in
    // embedding space IS the summary's embedding (no re-embedding), and
    // `code_raptor_search` does cosine ANN against it. Small per project
    // (k≈3-24 rows), so no HNSW index is needed — a sequential `<=>` scan
    // over the whole table is sub-millisecond.
    // ================================================================
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS code_summary_tree (
            id                BIGSERIAL PRIMARY KEY,
            project_id        INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            level             INTEGER NOT NULL DEFAULT 1,
            summary_text      TEXT NOT NULL,
            summary_embedding vector(1024) NOT NULL,
            member_count      INTEGER NOT NULL DEFAULT 0,
            member_paths      TEXT[] NOT NULL DEFAULT '{}',
            top_topics        TEXT[] NOT NULL DEFAULT '{}',
            computed_at       TIMESTAMPTZ DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_code_summary_tree_project
            ON code_summary_tree (project_id, level)",
    )
    .execute(pool)
    .await?;

    // ================================================================
    // Offline vulnerability advisories (graph-roadmap Phase 4.5)
    // ----------------------------------------------------------------
    // Populated OUT-OF-BAND by `pgmcp import-advisories <osv-dump>` — a local
    // OSV/GHSA dump import, never a runtime network fetch (local-only posture).
    // One row per (advisory, affected package, version range);
    // `cve_supply_chain` matches the parsed dependency inventory against these
    // by SemVer range.
    // ================================================================
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS vuln_advisories (
            id            BIGSERIAL PRIMARY KEY,
            advisory_id   TEXT NOT NULL,
            ecosystem     TEXT NOT NULL,
            package       TEXT NOT NULL,
            introduced    TEXT,
            fixed         TEXT,
            last_affected TEXT,
            severity      TEXT,
            summary       TEXT,
            imported_at   TIMESTAMPTZ DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_vuln_advisories_eco_pkg
            ON vuln_advisories (ecosystem, package)",
    )
    .execute(pool)
    .await?;

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
        // `source_symbol_id` has an `ON DELETE SET NULL` FK to file_symbols(id)
        // but was previously unindexed. Without this, every
        // `DELETE FROM file_symbols WHERE file_id = $1` in the symbol-extraction
        // cron forces Postgres to seq-scan all of symbol_references per deleted
        // row to enforce the SET NULL action — the cause of thousands of
        // "slow statement" WARNs and the symbol-extraction statement-timeout
        // cancellations. Partial (most rows are non-NULL) mirrors
        // idx_cge_source_symbol below.
        "CREATE INDEX IF NOT EXISTS idx_symbol_refs_source_symbol ON symbol_references(source_symbol_id) WHERE source_symbol_id IS NOT NULL",
    ];
    for idx_sql in &symbol_refs_indexes {
        sqlx::query(idx_sql).execute(pool).await?;
    }

    // ================================================================
    // SOTA Phase 1 — Per-function metrics (G1)
    //
    // One row per `file_symbols` row of kind='function'. Populated by the
    // `function-metrics` cron (src/cron/function_metrics.rs) after each
    // symbol-extraction pass. CASCADE delete with file_symbols, so reindex
    // invalidates derived metrics automatically.
    //
    // CC = McCabe cyclomatic complexity; cognitive = Sonar cognitive
    // complexity; halstead_* = vocabulary/length counts feeding Volume,
    // Difficulty, Effort, Bugs; NPath product of decision branches (capped
    // at i64::MAX with overflow flag); MI = Maintainability Index
    // clamped to [0, 100]; fan_in/fan_out filled by call-graph cron.
    // ================================================================
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS function_metrics (
            function_id BIGINT PRIMARY KEY REFERENCES file_symbols(id) ON DELETE CASCADE,
            file_id BIGINT NOT NULL REFERENCES indexed_files(id) ON DELETE CASCADE,
            project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            cyclomatic INTEGER NOT NULL DEFAULT 0,
            cognitive INTEGER NOT NULL DEFAULT 0,
            halstead_n1 INTEGER NOT NULL DEFAULT 0,
            halstead_n2 INTEGER NOT NULL DEFAULT 0,
            halstead_big_n1 INTEGER NOT NULL DEFAULT 0,
            halstead_big_n2 INTEGER NOT NULL DEFAULT 0,
            halstead_volume DOUBLE PRECISION NOT NULL DEFAULT 0.0,
            halstead_difficulty DOUBLE PRECISION NOT NULL DEFAULT 0.0,
            halstead_effort DOUBLE PRECISION NOT NULL DEFAULT 0.0,
            halstead_bugs DOUBLE PRECISION NOT NULL DEFAULT 0.0,
            npath BIGINT NOT NULL DEFAULT 1,
            npath_overflow BOOLEAN NOT NULL DEFAULT FALSE,
            loc INTEGER NOT NULL DEFAULT 0,
            comment_lines INTEGER NOT NULL DEFAULT 0,
            maintainability_index DOUBLE PRECISION NOT NULL DEFAULT 100.0,
            fan_in INTEGER NOT NULL DEFAULT 0,
            fan_out INTEGER NOT NULL DEFAULT 0,
            panic_paths INTEGER NOT NULL DEFAULT 0,
            unsafe_blocks INTEGER NOT NULL DEFAULT 0,
            computed_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await?;

    let function_metrics_indexes = [
        "CREATE INDEX IF NOT EXISTS idx_function_metrics_file ON function_metrics(file_id)",
        "CREATE INDEX IF NOT EXISTS idx_function_metrics_project ON function_metrics(project_id)",
        "CREATE INDEX IF NOT EXISTS idx_function_metrics_cyclomatic_desc ON function_metrics(project_id, cyclomatic DESC)",
        "CREATE INDEX IF NOT EXISTS idx_function_metrics_cognitive_desc ON function_metrics(project_id, cognitive DESC)",
        "CREATE INDEX IF NOT EXISTS idx_function_metrics_mi_asc ON function_metrics(project_id, maintainability_index ASC)",
    ];
    for idx_sql in &function_metrics_indexes {
        sqlx::query(idx_sql).execute(pool).await?;
    }

    // Graph-roadmap Phase 1.1 — function-level centralities. Materialized by
    // the call-graph cron (src/cron/call_graph.rs) once it builds the in-memory
    // CallGraph and runs the (now generic) PageRank / Brandes / Louvain / k-core
    // / harmonic algorithms on it. Additive ADD COLUMN IF NOT EXISTS so existing
    // installs migrate in place. These columns are OWNED by the call-graph cron;
    // upsert_function_metrics_batch must never list them in its ON CONFLICT DO
    // UPDATE clause, or a metrics pass would clobber them back to defaults.
    // community_id = -1 means "no community computed yet".
    let function_metrics_centrality_columns = [
        "ALTER TABLE function_metrics ADD COLUMN IF NOT EXISTS pagerank DOUBLE PRECISION NOT NULL DEFAULT 0.0",
        "ALTER TABLE function_metrics ADD COLUMN IF NOT EXISTS betweenness DOUBLE PRECISION NOT NULL DEFAULT 0.0",
        "ALTER TABLE function_metrics ADD COLUMN IF NOT EXISTS community_id INTEGER NOT NULL DEFAULT -1",
        "ALTER TABLE function_metrics ADD COLUMN IF NOT EXISTS coreness INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE function_metrics ADD COLUMN IF NOT EXISTS harmonic DOUBLE PRECISION NOT NULL DEFAULT 0.0",
    ];
    for col_sql in &function_metrics_centrality_columns {
        sqlx::query(col_sql).execute(pool).await?;
    }
    let function_metrics_centrality_indexes = [
        "CREATE INDEX IF NOT EXISTS idx_function_metrics_pagerank_desc ON function_metrics(project_id, pagerank DESC)",
        "CREATE INDEX IF NOT EXISTS idx_function_metrics_betweenness_desc ON function_metrics(project_id, betweenness DESC)",
        "CREATE INDEX IF NOT EXISTS idx_function_metrics_coreness_desc ON function_metrics(project_id, coreness DESC)",
    ];
    for idx_sql in &function_metrics_centrality_indexes {
        sqlx::query(idx_sql).execute(pool).await?;
    }

    // ================================================================
    // SOTA Phase 1 — Symbol-resolved call graph (G2)
    //
    // Extends `code_graph_edges` with symbol-level endpoints. Rows with
    // edge_type='call' MUST have source_symbol_id set; target_symbol_id
    // may be NULL (unresolved external call, in which case target_raw
    // holds the unresolved identifier).
    //
    // Decision (vs. parallel call_edges table): keep edges polymorphic so
    // existing PageRank / betweenness / community-detection tools that
    // filter on edge_type get call-graph variants for free.
    // ================================================================
    sqlx::query(
        "ALTER TABLE code_graph_edges ADD COLUMN IF NOT EXISTS source_symbol_id BIGINT REFERENCES file_symbols(id) ON DELETE SET NULL",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "ALTER TABLE code_graph_edges ADD COLUMN IF NOT EXISTS target_symbol_id BIGINT REFERENCES file_symbols(id) ON DELETE SET NULL",
    )
    .execute(pool)
    .await?;

    let cge_symbol_indexes = [
        "CREATE INDEX IF NOT EXISTS idx_cge_source_symbol ON code_graph_edges(source_symbol_id) WHERE source_symbol_id IS NOT NULL",
        "CREATE INDEX IF NOT EXISTS idx_cge_target_symbol ON code_graph_edges(target_symbol_id) WHERE target_symbol_id IS NOT NULL",
    ];
    for idx_sql in &cge_symbol_indexes {
        sqlx::query(idx_sql).execute(pool).await?;
    }

    // Idempotent CHECK install — Postgres has no `ADD CONSTRAINT IF NOT EXISTS`
    // so we DROP-then-ADD inside a single transaction to make re-runs cheap.
    sqlx::query(
        "ALTER TABLE code_graph_edges DROP CONSTRAINT IF EXISTS cge_call_needs_source_symbol",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "ALTER TABLE code_graph_edges ADD CONSTRAINT cge_call_needs_source_symbol CHECK (edge_type <> 'call' OR source_symbol_id IS NOT NULL)",
    )
    .execute(pool)
    .await?;

    // Re-tighten the source_symbol_id FK from ON DELETE SET NULL to ON DELETE
    // CASCADE. The original SET NULL semantics conflict with the CHECK above:
    // when a `file_symbols` row is deleted, the cascade tries to NULL out
    // `source_symbol_id` on any call-edge that referenced it, which the CHECK
    // immediately rejects — failing the parent DELETE transaction. This was
    // observed in production as ~180/day "Symbol extraction failed for file
    // (skipping)" warnings from `pgmcp::cron::symbol_extraction`. The
    // semantically correct response is CASCADE: a call edge whose source
    // symbol no longer exists is meaningless and should be removed too. The
    // `target_symbol_id` FK keeps SET NULL because calls to external /
    // unresolved symbols still carry useful information via `target_raw`.
    //
    // We look up the FK name and current ON DELETE action dynamically from
    // `pg_constraint` rather than relying on the auto-generated
    // `<table>_<col>_fkey` form, because some installs may have renamed
    // it. The DO block is idempotent: it only rewrites the FK when the
    // current action is NOT `c` (CASCADE), so re-running a daemon with an
    // already-fixed DB is a no-op.
    //
    // `confdeltype` values per Postgres docs:
    //   a = no action, r = restrict, c = cascade, n = set null, d = set default
    sqlx::query(
        "DO $$
         DECLARE
            con_name      TEXT;
            con_deltype   CHAR(1);
         BEGIN
            SELECT conname, confdeltype INTO con_name, con_deltype
              FROM pg_constraint c
              JOIN pg_class t   ON t.oid = c.conrelid
              JOIN pg_attribute a
                ON a.attrelid = c.conrelid
               AND a.attnum   = ANY (c.conkey)
             WHERE t.relname = 'code_graph_edges'
               AND a.attname = 'source_symbol_id'
               AND c.contype = 'f'
             LIMIT 1;
            IF con_name IS NOT NULL AND con_deltype <> 'c' THEN
                EXECUTE format('ALTER TABLE code_graph_edges DROP CONSTRAINT %I', con_name);
                ALTER TABLE code_graph_edges
                    ADD CONSTRAINT code_graph_edges_source_symbol_id_fkey
                    FOREIGN KEY (source_symbol_id)
                    REFERENCES file_symbols(id)
                    ON DELETE CASCADE;
            END IF;
         END $$;",
    )
    .execute(pool)
    .await?;

    // ================================================================
    // A2A (Agent-to-Agent) protocol tables
    //
    // Implements a substantive subset of Google's A2A spec
    // (https://google.github.io/A2A/) so external agents (Claude Code,
    // Codex CLI, etc.) can discover pgmcp's capabilities, submit Tasks,
    // and receive streamed events.
    // ================================================================
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS a2a_agents (
            id BIGSERIAL PRIMARY KEY,
            name TEXT NOT NULL UNIQUE,
            version TEXT NOT NULL,
            description TEXT,
            url TEXT NOT NULL,
            capabilities JSONB NOT NULL DEFAULT '{}'::jsonb,
            skills JSONB NOT NULL DEFAULT '[]'::jsonb,
            auth_schemes JSONB NOT NULL DEFAULT '[]'::jsonb,
            registered_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            last_seen_at TIMESTAMPTZ,
            specialty TEXT[] NOT NULL DEFAULT '{}',
            recommended_role TEXT
        )",
    )
    .execute(pool)
    .await?;
    // Upgrade-path for existing installs that pre-date specialty / role.
    sqlx::query(
        "ALTER TABLE a2a_agents
            ADD COLUMN IF NOT EXISTS specialty TEXT[] NOT NULL DEFAULT '{}'",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "ALTER TABLE a2a_agents
            ADD COLUMN IF NOT EXISTS recommended_role TEXT",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_a2a_agents_specialty
            ON a2a_agents USING GIN (specialty)",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS a2a_tasks (
            id UUID PRIMARY KEY,
            session_id UUID,
            requester_agent_id BIGINT REFERENCES a2a_agents(id) ON DELETE SET NULL,
            skill_id TEXT,
            status TEXT NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            completed_at TIMESTAMPTZ,
            error TEXT,
            push_notification_url TEXT,
            metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
            recursion_rounds INTEGER NOT NULL DEFAULT 1,
            current_round INTEGER NOT NULL DEFAULT 0,
            parent_task_id UUID REFERENCES a2a_tasks(id) ON DELETE SET NULL
        )",
    )
    .execute(pool)
    .await?;
    // Upgrade-path for existing installs.
    sqlx::query(
        "ALTER TABLE a2a_tasks
            ADD COLUMN IF NOT EXISTS recursion_rounds INTEGER NOT NULL DEFAULT 1",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "ALTER TABLE a2a_tasks
            ADD COLUMN IF NOT EXISTS current_round INTEGER NOT NULL DEFAULT 0",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "ALTER TABLE a2a_tasks
            ADD COLUMN IF NOT EXISTS parent_task_id UUID
                REFERENCES a2a_tasks(id) ON DELETE SET NULL",
    )
    .execute(pool)
    .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_a2a_tasks_status ON a2a_tasks(status)")
        .execute(pool)
        .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_a2a_tasks_session ON a2a_tasks(session_id)")
        .execute(pool)
        .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_a2a_tasks_parent ON a2a_tasks(parent_task_id)")
        .execute(pool)
        .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS a2a_messages (
            id BIGSERIAL PRIMARY KEY,
            task_id UUID NOT NULL REFERENCES a2a_tasks(id) ON DELETE CASCADE,
            role TEXT NOT NULL,
            parts JSONB NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            sequence INTEGER NOT NULL
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_a2a_messages_task ON a2a_messages(task_id, sequence)",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS a2a_artifacts (
            id BIGSERIAL PRIMARY KEY,
            task_id UUID NOT NULL REFERENCES a2a_tasks(id) ON DELETE CASCADE,
            name TEXT,
            parts JSONB NOT NULL,
            artifact_index INTEGER NOT NULL DEFAULT 0,
            append BOOLEAN NOT NULL DEFAULT FALSE,
            last_chunk BOOLEAN NOT NULL DEFAULT FALSE,
            metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            recursion_round INTEGER NOT NULL DEFAULT 0
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "ALTER TABLE a2a_artifacts
            ADD COLUMN IF NOT EXISTS recursion_round INTEGER NOT NULL DEFAULT 0",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS a2a_events (
            id BIGSERIAL PRIMARY KEY,
            task_id UUID NOT NULL REFERENCES a2a_tasks(id) ON DELETE CASCADE,
            kind TEXT NOT NULL,
            payload JSONB NOT NULL,
            sequence INTEGER NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_a2a_events_task ON a2a_events(task_id, sequence)")
        .execute(pool)
        .await?;

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
    // OCR extraction cache (Tesseract fallback for scanned PDFs)
    //
    // Keyed on xxh3_64 of the SOURCE PDF BYTES (not the extracted text)
    // so cache hits work *before* re-running pdftoppm + tesseract. The
    // hash matches across copies of the same PDF stored under different
    // paths (papers/ folder, workspace clones, HTTP-fetched temp files
    // from refresh_pattern_catalog). See src/indexer/extract/ocr_cache.rs.
    // ================================================================
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS ocr_extractions (
            content_hash BIGINT PRIMARY KEY,
            ocr_text     TEXT      NOT NULL,
            pages_ocred  INTEGER   NOT NULL,
            dpi          INTEGER   NOT NULL,
            languages    TEXT[]    NOT NULL,
            created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_ocr_extractions_created_at \
         ON ocr_extractions(created_at)",
    )
    .execute(pool)
    .await?;

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

    // ================================================================
    // MCP tool-call telemetry (per-call durable row)
    //
    // Append-only audit trail of every MCP tool invocation: tool name,
    // caller identity (lowercased rmcp clientInfo.name + version + MCP
    // protocol version), per-call duration, outcome (ok/error/timeout),
    // and an optional project tag (the value of the `project` parameter
    // when the tool accepts one). Privacy posture mirrors session_prompts:
    // tool/client names are stored verbatim; raw params never are — only
    // a sha256 of the canonicalized params JSON, populated when the
    // wrapper has access to it.
    //
    // Retention is enforced by the `telemetry-retention` cron job
    // (`src/cron/telemetry_retention.rs`), which deletes rows older than
    // `MetricsConfig::telemetry_retention_days` (default 30).
    // ================================================================
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS mcp_tool_calls (
            id               BIGSERIAL PRIMARY KEY,
            ts               TIMESTAMPTZ NOT NULL DEFAULT now(),
            tool             TEXT NOT NULL,
            client_name      TEXT NOT NULL,
            client_version   TEXT,
            protocol_version TEXT,
            mcp_session_id   TEXT,
            project          TEXT,
            project_id       INTEGER REFERENCES projects(id) ON DELETE SET NULL,
            cwd              TEXT,
            duration_ms      INTEGER NOT NULL,
            outcome          TEXT NOT NULL,
            error_class      TEXT,
            request_id       TEXT,
            params_sha256    TEXT,
            CHECK (outcome IN ('ok', 'error', 'timeout', 'cancelled'))
        )",
    )
    .execute(pool)
    .await?;

    let mcp_tool_calls_indexes = [
        "CREATE INDEX IF NOT EXISTS idx_mcp_tool_calls_ts ON mcp_tool_calls(ts)",
        "CREATE INDEX IF NOT EXISTS idx_mcp_tool_calls_tool_ts ON mcp_tool_calls(tool, ts)",
        "CREATE INDEX IF NOT EXISTS idx_mcp_tool_calls_client_ts ON mcp_tool_calls(client_name, ts)",
        "CREATE INDEX IF NOT EXISTS idx_mcp_tool_calls_project ON mcp_tool_calls(project_id) WHERE project_id IS NOT NULL",
    ];
    for idx in mcp_tool_calls_indexes {
        sqlx::query(idx).execute(pool).await?;
    }

    // ================================================================
    // Memory-server Phase 1: parallel 1024d embedding columns.
    //
    // `embedding_v2 VECTOR(1024)` lives alongside the legacy 384d
    // `embedding` column on `file_chunks` and `session_prompts` for the
    // duration of the BGE-M3 cutover. The Phase 1 embedding-migration
    // cron (`src/cron/embedding_migration.rs`) populates `embedding_v2`
    // incrementally; when both tables have zero unmigrated rows the
    // operator flips `pgmcp_metadata.active_embedding_signature` from
    // `minilm-l6-v2` to `bge-m3-v1` to route reads to the new column.
    // The old column is dropped in a separate cleanup migration after
    // one release of soak time.
    //
    // `embedding_signature TEXT` stamps each row with the model that
    // produced it so a mixed-signature transition window cannot silently
    // mis-rank cosine distances.
    //
    // HNSW index `idx_file_chunks_embedding_v2` / `_session_prompts_*`
    // is rebuilt only when `[vector]` params or signature change — same
    // pattern as `ensure_hnsw_index` / `ensure_session_prompts_hnsw_index`.
    // ================================================================
    ensure_memory_v2_columns(pool).await?;
    ensure_memory_v2_hnsw_index(pool, vector_config).await?;
    ensure_active_embedding_signature(pool).await?;
    // Phase 7: topic_dendrograms table for the hierarchical-
    // agglomerative + c-TF-IDF cron output. One row per project;
    // upserted by `cron::topic_dendrogram::run_project`.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS topic_dendrograms (
            project_id INTEGER PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,
            dendrogram_blob BYTEA NOT NULL,
            ctfidf_keywords JSONB NOT NULL,
            generated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await?;

    // ================================================================
    // Memory-server Phase 2: knowledge-graph tables.
    //
    // Scope tuple, bi-temporal entities/observations/relations, M:N
    // join tables for scope and cognitive tier, code-graph anchor,
    // RAPTOR summary tree (Phase 6.1, reserved), forget audit log
    // (Phase 8), reflection-run bookkeeping (Phase 5). See
    // `docs/memory-server/05-schema.md` §12.2 for the SQL contract this
    // function implements.
    //
    // All Phase-2 tables ship together so the bi-temporal invariants
    // (valid_from/valid_to/superseded_by chains) and FK relations are
    // coherent at migration completion.
    // ================================================================
    ensure_memory_phase2_tables(pool).await?;
    ensure_memory_phase2_hnsw_index(pool, vector_config).await?;

    // ================================================================
    // Memory-server Phase 6.3: heterogeneous-node graph view
    // (NodeRAG-inspired). UNION ALL projection across the existing
    // node-typed tables. See `docs/memory-server/05-schema.md` §12.3.
    // ================================================================
    ensure_memory_unified_views(pool, vector_config).await?;

    // Record the baseline. From this point on, future migration steps
    // can call `apply_step(pool, N, ...)`-style logic to land changes
    // that need transactional, exactly-once semantics. The pre-version-1
    // body above stays inline because every statement is already
    // idempotent and the body bundles cross-cutting concerns (HNSW
    // rebuilds keyed off `pgmcp_metadata`, conditional column adds).
    if !initial_schema_done {
        record_version(pool, INITIAL_SCHEMA_VERSION, "initial_schema").await?;
        info!(
            version = INITIAL_SCHEMA_VERSION,
            "initial schema migration recorded"
        );
    }

    // ================================================================
    // Migration step 2 — shadow_asr_v1
    // Unified semantic representation: type_tag_catalog, effect_catalog,
    // symbol_parameters, symbol_effects, additive columns on file_symbols
    // and symbol_references. See ADR-003 and `src/db/migrations/v2_shadow_asr.rs`.
    // ================================================================
    if !version_applied(pool, v2_shadow_asr::SHADOW_ASR_V1).await? {
        v2_shadow_asr::apply(pool).await?;
        record_version(
            pool,
            v2_shadow_asr::SHADOW_ASR_V1,
            v2_shadow_asr::SHADOW_ASR_V1_NAME,
        )
        .await?;
        info!(
            version = v2_shadow_asr::SHADOW_ASR_V1,
            "shadow_asr_v1 migration applied"
        );
    }

    // ================================================================
    // Migration step 3 — cross_language_signatures_v1
    // Materialized cross-language clone table powering
    // `mcp__pgmcp__cross_language_api_equivalents` and downstream
    // similarity tools.
    // ================================================================
    if !version_applied(
        pool,
        v3_cross_language_signatures::CROSS_LANGUAGE_SIGNATURES_V1,
    )
    .await?
    {
        v3_cross_language_signatures::apply(pool).await?;
        record_version(
            pool,
            v3_cross_language_signatures::CROSS_LANGUAGE_SIGNATURES_V1,
            v3_cross_language_signatures::CROSS_LANGUAGE_SIGNATURES_V1_NAME,
        )
        .await?;
        info!(
            version = v3_cross_language_signatures::CROSS_LANGUAGE_SIGNATURES_V1,
            "cross_language_signatures_v1 migration applied"
        );
    }

    Ok(())
}

/// Phase 5 C10 — extend the matview to cover every embedding-
/// bearing table populated by the full BGE-M3 migration. Adds:
/// - `commit_chunk` arm (git_commit_chunks.embedding_v2)
/// - `pattern_chunk` arm (software_pattern_chunks.embedding_v2)
/// - `session_mandate` arm (session_mandates.embedding, 1024d-direct)
///
/// The pre-Phase-5 `commit` arm (git_commits, no embedding) stays;
/// it surfaces commit subjects as labels for graph traversal.
///
/// Promoted to a `const` (F9) so its definition is the single source
/// of truth for the rebuild-gate hash. Edits propagate transparently
/// — the hash changes, the next restart rebuilds, the new hash is
/// upserted into `pgmcp_metadata['memory_unified_views_def_hash']`.
const MEMORY_UNIFIED_NODES_SQL: &str = "CREATE MATERIALIZED VIEW memory_unified_nodes AS
    SELECT 'memory_entity:' || id::TEXT AS node_id,
           'memory_entity'::TEXT AS node_type,
           name AS label,
           NULL::VECTOR(1024) AS embedding,
           importance
      FROM memory_entities WHERE valid_to IS NULL
    UNION ALL
    SELECT 'observation:' || id::TEXT, 'observation',
           LEFT(content, 200), embedding, importance
      FROM memory_observations WHERE valid_to IS NULL
    UNION ALL
    SELECT 'chunk:' || id::TEXT, 'chunk',
           LEFT(content, 200), embedding_v2, 0.5
      FROM file_chunks
      WHERE embedding_v2 IS NOT NULL
    UNION ALL
    SELECT 'topic:' || id::TEXT, 'topic',
           label, NULL::VECTOR(1024), 0.5
      FROM code_topics
    UNION ALL
    SELECT 'durable_mandate:' || id::TEXT, 'durable_mandate',
           imperative, embedding, 0.7
      FROM durable_mandates
    UNION ALL
    SELECT 'session_mandate:' || id::TEXT, 'session_mandate',
           imperative, embedding, 0.5
      FROM session_mandates
      WHERE embedding IS NOT NULL
    UNION ALL
    SELECT 'commit:' || id::TEXT, 'commit',
           subject, NULL::VECTOR(1024), 0.5
      FROM git_commits
    UNION ALL
    SELECT 'commit_chunk:' || id::TEXT, 'commit_chunk',
           LEFT(content, 200), embedding_v2, 0.4
      FROM git_commit_chunks
      WHERE embedding_v2 IS NOT NULL
    UNION ALL
    SELECT 'pattern_chunk:' || id::TEXT, 'pattern_chunk',
           LEFT(content, 200), embedding_v2, 0.6
      FROM software_pattern_chunks
      WHERE embedding_v2 IS NOT NULL";

/// Edges-view definition. Same single-source-of-truth posture as
/// `MEMORY_UNIFIED_NODES_SQL` — F9's hash-gate covers both.
const MEMORY_UNIFIED_EDGES_SQL: &str = "CREATE VIEW memory_unified_edges AS
    SELECT 'memory_entity:' || from_entity_id::TEXT AS from_id,
           'memory_entity'::TEXT AS from_type,
           'memory_entity:' || to_entity_id::TEXT AS to_id,
           'memory_entity'::TEXT AS to_type,
           relation_type AS edge_type,
           importance::DOUBLE PRECISION AS weight
      FROM memory_relations WHERE valid_to IS NULL
    UNION ALL
    SELECT 'memory_entity:' || entity_id::TEXT,
           'memory_entity',
           CASE
             WHEN file_id IS NOT NULL THEN 'chunk:' || file_id::TEXT
             WHEN chunk_id IS NOT NULL THEN 'chunk:' || chunk_id::TEXT
             ELSE 'topic:' || topic_id::TEXT
           END,
           CASE
             WHEN file_id IS NOT NULL THEN 'chunk'
             WHEN chunk_id IS NOT NULL THEN 'chunk'
             ELSE 'topic'
           END,
           anchor_type,
           1.0::DOUBLE PRECISION
      FROM memory_code_anchor
    UNION ALL
    SELECT 'chunk:' || chunk_id::TEXT,
           'chunk',
           'topic:' || topic_id::TEXT,
           'topic',
           'belongs_to',
           membership_score
      FROM chunk_topic_assignments
      WHERE membership_score >= 0.05";

/// `pgmcp_metadata` key storing the xxh3 hash of the combined matview
/// and edges-view CREATE SQL. F9 gate skips the rebuild when the stored
/// hash matches the current hash, avoiding ~35s of redundant matview
/// rebuild and HNSW index build on every daemon restart.
const MEMORY_UNIFIED_VIEWS_HASH_KEY: &str = "memory_unified_views_def_hash";

/// Phase 6.3: materialized `memory_unified_nodes` view +
/// `memory_unified_edges` view. F9: drops and recreates only when
/// the combined definition (`MEMORY_UNIFIED_NODES_SQL` +
/// `MEMORY_UNIFIED_EDGES_SQL`) has changed since the last successful
/// rebuild. The hash is keyed in `pgmcp_metadata` so schema changes
/// still take effect on the next restart automatically.
async fn ensure_memory_unified_views(
    pool: &PgPool,
    config: &VectorConfig,
) -> Result<(), sqlx::Error> {
    let combined = format!(
        "{}\n---\n{}",
        MEMORY_UNIFIED_NODES_SQL, MEMORY_UNIFIED_EDGES_SQL
    );
    let current_hash = format!("{:016x}", xxhash_rust::xxh3::xxh3_64(combined.as_bytes()));

    let stored: Option<String> =
        sqlx::query_scalar("SELECT value FROM pgmcp_metadata WHERE key = $1")
            .bind(MEMORY_UNIFIED_VIEWS_HASH_KEY)
            .fetch_optional(pool)
            .await?;

    if stored.as_deref() == Some(current_hash.as_str()) {
        info!(
            "memory_unified_views definition unchanged (hash {}); skipping rebuild",
            current_hash
        );
        return Ok(());
    }

    info!(
        "memory_unified_views definition changed (was {:?}, now {}); rebuilding",
        stored, current_hash
    );

    // Drop the matview if it already exists so we can rebuild against
    // the latest column shapes. Order matters: drop the view first
    // (it depends on the matview indirectly through underlying tables
    // only, but explicit drops keep refresh semantics simple).
    sqlx::query("DROP VIEW IF EXISTS memory_unified_edges")
        .execute(pool)
        .await?;
    sqlx::query("DROP MATERIALIZED VIEW IF EXISTS memory_unified_nodes")
        .execute(pool)
        .await?;

    sqlx::query(MEMORY_UNIFIED_NODES_SQL).execute(pool).await?;
    // Lookup index by (node_type, node_id-suffix prefix) for the
    // neighbors / search paths. Cheap b-tree; the HNSW would be on
    // `embedding` but a matview supports HNSW only if pgvector is
    // recent enough — we keep the cosine index implicit (matview is
    // rebuilt on refresh, not incrementally).
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_memory_unified_nodes_type
            ON memory_unified_nodes (node_type)",
    )
    .execute(pool)
    .await?;
    // HNSW on embedding for vector retrieval. Built via the F8
    // helper so `maintenance_work_mem` / `statement_timeout` /
    // parallel-workers tuning kicks in.
    build_hnsw_index(
        pool,
        config,
        "CREATE INDEX IF NOT EXISTS idx_memory_unified_nodes_embedding
            ON memory_unified_nodes USING hnsw (embedding vector_cosine_ops)
            WITH (m = 24, ef_construction = 200)",
    )
    .await?;

    sqlx::query(MEMORY_UNIFIED_EDGES_SQL).execute(pool).await?;

    sqlx::query(
        "INSERT INTO pgmcp_metadata (key, value)
         VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(MEMORY_UNIFIED_VIEWS_HASH_KEY)
    .bind(&current_hash)
    .execute(pool)
    .await?;

    Ok(())
}

/// Phase 2: knowledge-graph base tables, enums, indices, and CHECK
/// constraints. Idempotent — drop+recreate is avoided so existing rows
/// survive re-migration; new tables get `CREATE TABLE IF NOT EXISTS`.
async fn ensure_memory_phase2_tables(pool: &PgPool) -> Result<(), sqlx::Error> {
    // ENUM types. `IF NOT EXISTS` for types arrived in Postgres 14; the
    // fallback for older clusters uses pg_catalog probing.
    let enum_stmts = [
        (
            "memory_tier",
            "CREATE TYPE memory_tier AS ENUM ('working','episodic','semantic','procedural','reflective')",
        ),
        (
            "memory_source",
            "CREATE TYPE memory_source AS ENUM ('user_explicit','llm_extraction','reflection','consolidation','agent_write','migration')",
        ),
        (
            "memory_outcome",
            "CREATE TYPE memory_outcome AS ENUM ('worked','failed','mixed','prefer','avoid','superseded_by_peer')",
        ),
    ];
    for (name, create_sql) in enum_stmts {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_type WHERE typname = $1)")
                .bind(name)
                .fetch_one(pool)
                .await?;
        if !exists {
            sqlx::query(create_sql).execute(pool).await?;
        }
    }

    // Scope tuple. Each dimension nullable → NULL means "any". The
    // composite UNIQUE constraint relies on Postgres's
    // `NULLS NOT DISTINCT` (PG15+); for older servers, two NULLs would
    // still be considered distinct and the constraint wouldn't prevent
    // duplicates — at which point the `upsert_scope` helper in queries.rs
    // becomes the authoritative dedupe path.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS memory_scope (
            id          BIGSERIAL PRIMARY KEY,
            user_id     TEXT,
            agent_id    TEXT,
            session_id  UUID REFERENCES sessions(id) ON DELETE CASCADE,
            project_id  INTEGER REFERENCES projects(id) ON DELETE CASCADE,
            created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await?;
    // Unique-or-null tuple. We emit a `UNIQUE NULLS NOT DISTINCT` when
    // the server supports it (PG15+). On older servers we still create
    // a regular UNIQUE — duplicates are then disambiguated by the
    // `find_or_create_scope` query.
    let pg_version: i32 = sqlx::query_scalar("SHOW server_version_num")
        .fetch_one(pool)
        .await
        .map(|s: String| s.parse().unwrap_or(0))
        .unwrap_or(0);
    let unique_clause = if pg_version >= 150000 {
        "UNIQUE NULLS NOT DISTINCT"
    } else {
        "UNIQUE"
    };
    // ALTER TABLE ADD CONSTRAINT has no `IF NOT EXISTS` until Postgres 17,
    // so we pre-check `pg_constraint` and only issue the ALTER on first
    // run. The constraint name is project-stable, so this is exactly-once.
    let constraint_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM pg_constraint
            WHERE conname = 'memory_scope_tuple_uq'
        )",
    )
    .fetch_one(pool)
    .await?;
    if !constraint_exists {
        sqlx::query(&format!(
            "ALTER TABLE memory_scope
                ADD CONSTRAINT memory_scope_tuple_uq
                {} (user_id, agent_id, session_id, project_id)",
            unique_clause
        ))
        .execute(pool)
        .await?;
    }

    // Entities. Bi-temporal columns are NOT NULL on valid_from with a
    // sentinel default (NOW()); valid_to and superseded_by stay NULL
    // for the active row.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS memory_entities (
            id              BIGSERIAL PRIMARY KEY,
            name            TEXT NOT NULL,
            entity_type     TEXT NOT NULL,
            canonical_name  TEXT,
            importance      REAL NOT NULL DEFAULT 0.5,
            source          memory_source NOT NULL,
            created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            valid_from      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            valid_to        TIMESTAMPTZ,
            superseded_by   BIGINT REFERENCES memory_entities(id),
            UNIQUE (name, entity_type, valid_from)
        )",
    )
    .execute(pool)
    .await?;
    let entity_indexes = [
        "CREATE INDEX IF NOT EXISTS idx_memory_entities_active
            ON memory_entities (name, entity_type) WHERE valid_to IS NULL",
        "CREATE INDEX IF NOT EXISTS idx_memory_entities_temporal
            ON memory_entities (valid_from, valid_to)",
        "CREATE INDEX IF NOT EXISTS idx_memory_entities_canonical
            ON memory_entities (canonical_name) WHERE valid_to IS NULL",
    ];
    for s in entity_indexes {
        sqlx::query(s).execute(pool).await?;
    }

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS memory_entity_scope (
            entity_id  BIGINT NOT NULL REFERENCES memory_entities(id) ON DELETE CASCADE,
            scope_id   BIGINT NOT NULL REFERENCES memory_scope(id) ON DELETE CASCADE,
            PRIMARY KEY (entity_id, scope_id)
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_memory_entity_scope_scope
            ON memory_entity_scope (scope_id)",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS memory_entity_tier (
            entity_id  BIGINT NOT NULL REFERENCES memory_entities(id) ON DELETE CASCADE,
            tier       memory_tier NOT NULL,
            weight     REAL NOT NULL DEFAULT 1.0,
            PRIMARY KEY (entity_id, tier),
            CHECK (weight >= 0.0 AND weight <= 1.0)
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_memory_entity_tier_tier
            ON memory_entity_tier (tier)",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS memory_observations (
            id                    BIGSERIAL PRIMARY KEY,
            entity_id             BIGINT NOT NULL REFERENCES memory_entities(id) ON DELETE CASCADE,
            content               TEXT NOT NULL,
            content_sha256        CHAR(64) NOT NULL,
            embedding             vector(1024),
            embedding_signature   TEXT NOT NULL DEFAULT 'bge-m3-v1',
            importance            REAL NOT NULL DEFAULT 0.5,
            source                memory_source NOT NULL,
            source_session_id     UUID REFERENCES sessions(id),
            source_prompt_id      BIGINT REFERENCES session_prompts(id),
            derived_from          BIGINT[],
            created_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            valid_from            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            valid_to              TIMESTAMPTZ,
            superseded_by         BIGINT REFERENCES memory_observations(id),
            UNIQUE (entity_id, content_sha256, valid_from)
        )",
    )
    .execute(pool)
    .await?;
    let obs_indexes = [
        "CREATE INDEX IF NOT EXISTS idx_memory_observations_active
            ON memory_observations (entity_id) WHERE valid_to IS NULL",
        "CREATE INDEX IF NOT EXISTS idx_memory_observations_temporal
            ON memory_observations (valid_from, valid_to)",
        "CREATE INDEX IF NOT EXISTS idx_memory_observations_fts
            ON memory_observations USING gin (to_tsvector('english', content))",
    ];
    for s in obs_indexes {
        sqlx::query(s).execute(pool).await?;
    }

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS memory_relations (
            id              BIGSERIAL PRIMARY KEY,
            from_entity_id  BIGINT NOT NULL REFERENCES memory_entities(id) ON DELETE CASCADE,
            to_entity_id    BIGINT NOT NULL REFERENCES memory_entities(id) ON DELETE CASCADE,
            relation_type   TEXT NOT NULL,
            importance      REAL NOT NULL DEFAULT 0.5,
            source          memory_source NOT NULL,
            created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            valid_from      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            valid_to        TIMESTAMPTZ,
            superseded_by   BIGINT REFERENCES memory_relations(id),
            UNIQUE (from_entity_id, to_entity_id, relation_type, valid_from),
            CHECK (from_entity_id <> to_entity_id)
        )",
    )
    .execute(pool)
    .await?;
    let rel_indexes = [
        "CREATE INDEX IF NOT EXISTS idx_memory_relations_from
            ON memory_relations (from_entity_id) WHERE valid_to IS NULL",
        "CREATE INDEX IF NOT EXISTS idx_memory_relations_to
            ON memory_relations (to_entity_id) WHERE valid_to IS NULL",
        "CREATE INDEX IF NOT EXISTS idx_memory_relations_type
            ON memory_relations (relation_type) WHERE valid_to IS NULL",
        "CREATE INDEX IF NOT EXISTS idx_memory_relations_temporal
            ON memory_relations (valid_from, valid_to)",
    ];
    for s in rel_indexes {
        sqlx::query(s).execute(pool).await?;
    }

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS memory_code_anchor (
            id           BIGSERIAL PRIMARY KEY,
            entity_id    BIGINT NOT NULL REFERENCES memory_entities(id) ON DELETE CASCADE,
            file_id      BIGINT REFERENCES indexed_files(id) ON DELETE CASCADE,
            chunk_id     BIGINT REFERENCES file_chunks(id) ON DELETE CASCADE,
            topic_id     BIGINT REFERENCES code_topics(id) ON DELETE CASCADE,
            anchor_type  TEXT NOT NULL,
            created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            CHECK (file_id IS NOT NULL OR chunk_id IS NOT NULL OR topic_id IS NOT NULL)
        )",
    )
    .execute(pool)
    .await?;
    let anchor_indexes = [
        "CREATE INDEX IF NOT EXISTS idx_memory_code_anchor_entity ON memory_code_anchor (entity_id)",
        "CREATE INDEX IF NOT EXISTS idx_memory_code_anchor_file   ON memory_code_anchor (file_id)   WHERE file_id   IS NOT NULL",
        "CREATE INDEX IF NOT EXISTS idx_memory_code_anchor_chunk  ON memory_code_anchor (chunk_id)  WHERE chunk_id  IS NOT NULL",
        "CREATE INDEX IF NOT EXISTS idx_memory_code_anchor_topic  ON memory_code_anchor (topic_id)  WHERE topic_id  IS NOT NULL",
    ];
    for s in anchor_indexes {
        sqlx::query(s).execute(pool).await?;
    }

    // RAPTOR summary tree (Phase 6.1, reserved). Shipped with Phase 2
    // so all memory_* tables land in one migration; the cron that
    // populates it lands later.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS memory_summary_tree (
            id                BIGSERIAL PRIMARY KEY,
            scope_id          BIGINT NOT NULL REFERENCES memory_scope(id) ON DELETE CASCADE,
            level             INTEGER NOT NULL,
            parent_id         BIGINT REFERENCES memory_summary_tree(id),
            observation_id    BIGINT REFERENCES memory_observations(id),
            summary_text      TEXT,
            summary_embedding vector(1024),
            child_count       INTEGER,
            created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            CHECK ((level = 0 AND observation_id IS NOT NULL AND summary_text IS NULL)
                OR (level > 0 AND observation_id IS NULL     AND summary_text IS NOT NULL))
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_memory_summary_tree_level
            ON memory_summary_tree (scope_id, level)",
    )
    .execute(pool)
    .await?;

    // Forget audit log (Phase 8).
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS memory_forget_log (
            id             BIGSERIAL PRIMARY KEY,
            actor          TEXT NOT NULL,
            target_type    TEXT NOT NULL,
            target_id      BIGINT NOT NULL,
            cascade        BOOLEAN NOT NULL,
            rows_affected  INTEGER NOT NULL,
            manifest_json  JSONB,
            forgotten_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await?;

    // Reflection bookkeeping (Phase 5).
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS memory_reflection_runs (
            id                BIGSERIAL PRIMARY KEY,
            scope_id          BIGINT REFERENCES memory_scope(id) ON DELETE SET NULL,
            started_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            finished_at       TIMESTAMPTZ,
            observation_count INTEGER,
            facts_emitted     INTEGER,
            trigger           TEXT NOT NULL,
            CHECK (trigger IN ('agent','cron'))
        )",
    )
    .execute(pool)
    .await?;

    // A2A best-practice exchange (Part A). Authoritative, cheaply
    // aggregatable outcome ledger: one row per peer report about an
    // approach for a task-kind, mirrored into a memory_observation
    // (observation_id) so it also participates in PPR/unified retrieval
    // and reflection. Created here, after memory_observations, so the FK
    // resolves; a2a_tasks (created earlier in run_migrations) backs
    // parent_task_id.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS agent_outcomes (
            id              BIGSERIAL PRIMARY KEY,
            agent_id        TEXT NOT NULL,
            project_id      INTEGER REFERENCES projects(id) ON DELETE CASCADE,
            task_kind       TEXT NOT NULL,
            approach        TEXT NOT NULL,
            outcome         memory_outcome NOT NULL,
            confidence      REAL NOT NULL DEFAULT 0.5 CHECK (confidence >= 0.0 AND confidence <= 1.0),
            evidence        TEXT,
            parent_task_id  UUID REFERENCES a2a_tasks(id) ON DELETE SET NULL,
            observation_id  BIGINT REFERENCES memory_observations(id) ON DELETE SET NULL,
            created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await?;
    let outcome_indexes = [
        "CREATE INDEX IF NOT EXISTS idx_agent_outcomes_proj_kind
            ON agent_outcomes (project_id, task_kind)",
        "CREATE INDEX IF NOT EXISTS idx_agent_outcomes_agent
            ON agent_outcomes (agent_id)",
    ];
    for s in outcome_indexes {
        sqlx::query(s).execute(pool).await?;
    }

    // Per-agent trust prior — anti-flooding weight read by A4 promotion.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS agent_trust (
            agent_id          TEXT PRIMARY KEY,
            importance_prior  REAL NOT NULL DEFAULT 0.5 CHECK (importance_prior >= 0.0 AND importance_prior <= 1.0),
            reports_total     BIGINT NOT NULL DEFAULT 0,
            reports_promoted  BIGINT NOT NULL DEFAULT 0,
            updated_at        TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await?;

    // RLM trajectory recording (Part B phase B3). One row per recursive
    // decomposition run; `encoded_series` is the precomputed step→f64
    // sequence the MSM trajectory index (B4) compares. `success` is
    // back-filled by the outcome labeler joining agent_outcomes.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS agent_trajectories (
            id               BIGSERIAL PRIMARY KEY,
            task_id          UUID NOT NULL REFERENCES a2a_tasks(id) ON DELETE CASCADE,
            parent_task_id   UUID REFERENCES a2a_tasks(id) ON DELETE SET NULL,
            kind             TEXT NOT NULL DEFAULT 'rlm',
            environment      JSONB NOT NULL DEFAULT '{}'::jsonb,
            query_sha256     CHAR(64) NOT NULL,
            strategy         TEXT,
            depth_reached    INTEGER NOT NULL DEFAULT 1,
            total_subcalls   INTEGER NOT NULL DEFAULT 0,
            total_latency_ms BIGINT NOT NULL DEFAULT 0,
            success          BOOLEAN,
            self_grade       DOUBLE PRECISION,
            outcome_obs_id   BIGINT REFERENCES memory_observations(id) ON DELETE SET NULL,
            encoded_series   DOUBLE PRECISION[] NOT NULL DEFAULT '{}',
            created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await?;
    // E (closed MSM loop): the RLM self-grade in [0,1] from the verify
    // rubric. Idempotent add for installs created before Part E.
    sqlx::query(
        "ALTER TABLE agent_trajectories ADD COLUMN IF NOT EXISTS self_grade DOUBLE PRECISION",
    )
    .execute(pool)
    .await?;
    let traj_indexes = [
        "CREATE INDEX IF NOT EXISTS idx_agent_trajectories_task
            ON agent_trajectories (task_id)",
        "CREATE INDEX IF NOT EXISTS idx_agent_trajectories_success
            ON agent_trajectories (success) WHERE success IS NOT NULL",
    ];
    for s in traj_indexes {
        sqlx::query(s).execute(pool).await?;
    }
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS trajectory_steps (
            id            BIGSERIAL PRIMARY KEY,
            trajectory_id BIGINT NOT NULL REFERENCES agent_trajectories(id) ON DELETE CASCADE,
            ord           INTEGER NOT NULL,
            step_kind     TEXT NOT NULL,
            depth         INTEGER NOT NULL DEFAULT 0,
            latency_ms    BIGINT NOT NULL DEFAULT 0,
            est_tokens    BIGINT NOT NULL DEFAULT 0,
            success       BOOLEAN NOT NULL DEFAULT TRUE,
            UNIQUE (trajectory_id, ord)
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_trajectory_steps_traj
            ON trajectory_steps (trajectory_id, ord)",
    )
    .execute(pool)
    .await?;

    Ok(())
}

/// Phase 2 HNSW indices on `memory_observations.embedding` and
/// `memory_summary_tree.summary_embedding`. Rebuild guard mirrors the
/// existing `ensure_*_hnsw_index` helpers.
async fn ensure_memory_phase2_hnsw_index(
    pool: &PgPool,
    config: &VectorConfig,
) -> Result<(), sqlx::Error> {
    let current_params = format!(
        "m={},ef_construction={}",
        config.hnsw_m, config.hnsw_ef_construction
    );

    // memory_observations.embedding
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT value FROM pgmcp_metadata WHERE key = 'memory_observations_hnsw_params'",
    )
    .fetch_optional(pool)
    .await?;
    if stored.as_deref() != Some(&current_params) {
        sqlx::query("DROP INDEX IF EXISTS idx_memory_observations_embedding")
            .execute(pool)
            .await?;
        let create_sql = format!(
            "CREATE INDEX idx_memory_observations_embedding ON memory_observations \
             USING hnsw (embedding vector_cosine_ops) WITH (m = {}, ef_construction = {})",
            config.hnsw_m, config.hnsw_ef_construction
        );
        build_hnsw_index(pool, config, &create_sql).await?;
        sqlx::query(
            "INSERT INTO pgmcp_metadata (key, value)
             VALUES ('memory_observations_hnsw_params', $1)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(&current_params)
        .execute(pool)
        .await?;
    }

    // memory_summary_tree.summary_embedding
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT value FROM pgmcp_metadata WHERE key = 'memory_summary_tree_hnsw_params'",
    )
    .fetch_optional(pool)
    .await?;
    if stored.as_deref() != Some(&current_params) {
        sqlx::query("DROP INDEX IF EXISTS idx_memory_summary_tree_embedding")
            .execute(pool)
            .await?;
        let create_sql = format!(
            "CREATE INDEX idx_memory_summary_tree_embedding ON memory_summary_tree \
             USING hnsw (summary_embedding vector_cosine_ops) \
             WITH (m = {}, ef_construction = {})",
            config.hnsw_m, config.hnsw_ef_construction
        );
        build_hnsw_index(pool, config, &create_sql).await?;
        sqlx::query(
            "INSERT INTO pgmcp_metadata (key, value)
             VALUES ('memory_summary_tree_hnsw_params', $1)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(&current_params)
        .execute(pool)
        .await?;
    }

    Ok(())
}

/// Phase 1: add `embedding_v2 VECTOR(1024)` and `embedding_signature TEXT`
/// to `file_chunks` and `session_prompts`. Idempotent.
///
/// Phase 5 C1 extension: add the same parallel columns to
/// `git_commit_chunks` and `software_pattern_chunks` so the full BGE-M3
/// migration covers every code-side embedding table. Also drop the
/// `NOT NULL` constraint on the legacy `embedding` columns so the
/// indexer's mid-cutover dual-write (legacy zero-placeholder + real
/// v2 vector) can succeed.
async fn ensure_memory_v2_columns(pool: &PgPool) -> Result<(), sqlx::Error> {
    let stmts = [
        "ALTER TABLE file_chunks ADD COLUMN IF NOT EXISTS embedding_v2 vector(1024)",
        "ALTER TABLE file_chunks ADD COLUMN IF NOT EXISTS embedding_signature TEXT",
        // Graph-roadmap Phase 2.3: BGE-M3 learned-sparse (SPLADE-style) vector,
        // dimension = XLM-R vocab (250002). Nullable + UNINDEXED: the sparse
        // retrieval leg is bounded by the project/lang filter + per-leg LIMIT,
        // so a brute-force `<#>` scan is acceptable and we avoid pgvector's
        // sparsevec HNSW non-zero-dimension cap. Backfilled by the
        // embedding-migration cron; chunks without it fall back to dense+BM25.
        "ALTER TABLE file_chunks ADD COLUMN IF NOT EXISTS sparse_v2 sparsevec(250002)",
        // Graph-roadmap Phase 2.4 (Contextual Retrieval): the deterministic
        // situating prefix prepended to a chunk before embedding. NULL = not yet
        // contextualized; the cron drains those, re-embeds `embedding_v2` from
        // `contextual_text || content`, and stamps the prefix here. The raw
        // `content` returned to the agent is never modified.
        "ALTER TABLE file_chunks ADD COLUMN IF NOT EXISTS contextual_text TEXT",
        "ALTER TABLE session_prompts ADD COLUMN IF NOT EXISTS embedding_v2 vector(1024)",
        "ALTER TABLE session_prompts ADD COLUMN IF NOT EXISTS embedding_signature TEXT",
        "ALTER TABLE durable_mandates ADD COLUMN IF NOT EXISTS embedding vector(1024)",
        "ALTER TABLE durable_mandates ADD COLUMN IF NOT EXISTS embedding_signature TEXT",
        "ALTER TABLE session_mandates ADD COLUMN IF NOT EXISTS embedding vector(1024)",
        "ALTER TABLE session_mandates ADD COLUMN IF NOT EXISTS embedding_signature TEXT",
        // Phase 5 C1: parallel columns on the two remaining code-side
        // tables. Plan reference:
        // ~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md
        // Phase 5 C1.
        "ALTER TABLE git_commit_chunks ADD COLUMN IF NOT EXISTS embedding_v2 vector(1024)",
        "ALTER TABLE git_commit_chunks ADD COLUMN IF NOT EXISTS embedding_signature TEXT",
        "ALTER TABLE software_pattern_chunks ADD COLUMN IF NOT EXISTS embedding_v2 vector(1024)",
        "ALTER TABLE software_pattern_chunks ADD COLUMN IF NOT EXISTS embedding_signature TEXT",
        // (Legacy `embedding` DROP NOT NULL is handled in the guarded loop
        // below — it must tolerate the column being absent post-cutover.)
    ];
    for s in stmts {
        sqlx::query(s).execute(pool).await?;
    }

    // Phase 5 C1: drop NOT NULL on every legacy 384d `embedding` column so the
    // indexer dual-write (zero placeholder into legacy + real 1024d into
    // embedding_v2) succeeds during the migration window. GUARDED on column
    // presence: `embed-cutover --drop-legacy` permanently drops the column
    // post-soak (C12), and `ALTER COLUMN … DROP NOT NULL` has no `IF EXISTS`
    // form — so without this guard `run_migrations` would throw
    // `column "embedding" of relation "…" does not exist` on every boot of a
    // post-cutover database. `DROP NOT NULL` is itself idempotent when the
    // column exists, so the guard only needs to skip when it is ABSENT. The
    // table names are a fixed literal allowlist (not user input) ⇒ the
    // `format!` is injection-safe.
    for table in [
        "file_chunks",
        "session_prompts",
        "git_commit_chunks",
        "software_pattern_chunks",
    ] {
        if column_exists(pool, table, "embedding").await? {
            sqlx::query(&format!(
                "ALTER TABLE {table} ALTER COLUMN embedding DROP NOT NULL"
            ))
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

/// Phase 1: HNSW indices on the new 1024d `embedding_v2` columns. Built only
/// once and rebuilt when `[vector]` params change. Mirrors the rebuild guard
/// pattern from `ensure_hnsw_index`.
async fn ensure_memory_v2_hnsw_index(
    pool: &PgPool,
    config: &VectorConfig,
) -> Result<(), sqlx::Error> {
    let current_params = format!(
        "m={},ef_construction={}",
        config.hnsw_m, config.hnsw_ef_construction
    );

    // file_chunks.embedding_v2
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT value FROM pgmcp_metadata WHERE key = 'memory_v2_file_chunks_hnsw_params'",
    )
    .fetch_optional(pool)
    .await?;
    if stored.as_deref() != Some(&current_params) {
        sqlx::query("DROP INDEX IF EXISTS idx_file_chunks_embedding_v2")
            .execute(pool)
            .await?;
        let create_sql = format!(
            "CREATE INDEX idx_file_chunks_embedding_v2 ON file_chunks \
             USING hnsw (embedding_v2 vector_cosine_ops) \
             WITH (m = {}, ef_construction = {})",
            config.hnsw_m, config.hnsw_ef_construction
        );
        build_hnsw_index(pool, config, &create_sql).await?;
        sqlx::query(
            "INSERT INTO pgmcp_metadata (key, value)
             VALUES ('memory_v2_file_chunks_hnsw_params', $1)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(&current_params)
        .execute(pool)
        .await?;
    }

    // session_prompts.embedding_v2
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT value FROM pgmcp_metadata WHERE key = 'memory_v2_session_prompts_hnsw_params'",
    )
    .fetch_optional(pool)
    .await?;
    if stored.as_deref() != Some(&current_params) {
        sqlx::query("DROP INDEX IF EXISTS idx_session_prompts_embedding_v2")
            .execute(pool)
            .await?;
        let create_sql = format!(
            "CREATE INDEX idx_session_prompts_embedding_v2 ON session_prompts \
             USING hnsw (embedding_v2 vector_cosine_ops) \
             WITH (m = {}, ef_construction = {})",
            config.hnsw_m, config.hnsw_ef_construction
        );
        build_hnsw_index(pool, config, &create_sql).await?;
        sqlx::query(
            "INSERT INTO pgmcp_metadata (key, value)
             VALUES ('memory_v2_session_prompts_hnsw_params', $1)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(&current_params)
        .execute(pool)
        .await?;
    }

    // durable_mandates.embedding
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT value FROM pgmcp_metadata WHERE key = 'memory_v2_durable_mandates_hnsw_params'",
    )
    .fetch_optional(pool)
    .await?;
    if stored.as_deref() != Some(&current_params) {
        sqlx::query("DROP INDEX IF EXISTS idx_durable_mandates_embedding")
            .execute(pool)
            .await?;
        let create_sql = format!(
            "CREATE INDEX idx_durable_mandates_embedding ON durable_mandates \
             USING hnsw (embedding vector_cosine_ops) \
             WITH (m = {}, ef_construction = {})",
            config.hnsw_m, config.hnsw_ef_construction
        );
        build_hnsw_index(pool, config, &create_sql).await?;
        sqlx::query(
            "INSERT INTO pgmcp_metadata (key, value)
             VALUES ('memory_v2_durable_mandates_hnsw_params', $1)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(&current_params)
        .execute(pool)
        .await?;
    }

    // Phase 5 C1: git_commit_chunks.embedding_v2
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT value FROM pgmcp_metadata WHERE key = 'memory_v2_git_commit_chunks_hnsw_params'",
    )
    .fetch_optional(pool)
    .await?;
    if stored.as_deref() != Some(&current_params) {
        sqlx::query("DROP INDEX IF EXISTS idx_git_commit_chunks_embedding_v2")
            .execute(pool)
            .await?;
        let create_sql = format!(
            "CREATE INDEX idx_git_commit_chunks_embedding_v2 ON git_commit_chunks \
             USING hnsw (embedding_v2 vector_cosine_ops) \
             WITH (m = {}, ef_construction = {})",
            config.hnsw_m, config.hnsw_ef_construction
        );
        build_hnsw_index(pool, config, &create_sql).await?;
        sqlx::query(
            "INSERT INTO pgmcp_metadata (key, value)
             VALUES ('memory_v2_git_commit_chunks_hnsw_params', $1)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(&current_params)
        .execute(pool)
        .await?;
    }

    // Phase 5 C1: software_pattern_chunks.embedding_v2
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT value FROM pgmcp_metadata WHERE key = 'memory_v2_software_pattern_chunks_hnsw_params'",
    )
    .fetch_optional(pool)
    .await?;
    if stored.as_deref() != Some(&current_params) {
        sqlx::query("DROP INDEX IF EXISTS idx_software_pattern_chunks_embedding_v2")
            .execute(pool)
            .await?;
        let create_sql = format!(
            "CREATE INDEX idx_software_pattern_chunks_embedding_v2 ON software_pattern_chunks \
             USING hnsw (embedding_v2 vector_cosine_ops) \
             WITH (m = {}, ef_construction = {})",
            config.hnsw_m, config.hnsw_ef_construction
        );
        build_hnsw_index(pool, config, &create_sql).await?;
        sqlx::query(
            "INSERT INTO pgmcp_metadata (key, value)
             VALUES ('memory_v2_software_pattern_chunks_hnsw_params', $1)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(&current_params)
        .execute(pool)
        .await?;
    }

    // session_mandates.embedding — restores symmetry with durable_mandates
    // above. `ensure_memory_v2_columns` adds session_mandates.embedding
    // (vector(1024)) and the migration cron populates it, but the original
    // index builder shipped without this block, leaving session_mandates the
    // only embedding-bearing table with no ANN index.
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT value FROM pgmcp_metadata WHERE key = 'memory_v2_session_mandates_hnsw_params'",
    )
    .fetch_optional(pool)
    .await?;
    if stored.as_deref() != Some(&current_params) {
        sqlx::query("DROP INDEX IF EXISTS idx_session_mandates_embedding")
            .execute(pool)
            .await?;
        let create_sql = format!(
            "CREATE INDEX idx_session_mandates_embedding ON session_mandates \
             USING hnsw (embedding vector_cosine_ops) \
             WITH (m = {}, ef_construction = {})",
            config.hnsw_m, config.hnsw_ef_construction
        );
        build_hnsw_index(pool, config, &create_sql).await?;
        sqlx::query(
            "INSERT INTO pgmcp_metadata (key, value)
             VALUES ('memory_v2_session_mandates_hnsw_params', $1)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(&current_params)
        .execute(pool)
        .await?;
    }

    Ok(())
}

/// Phase 1: initialize the `active_embedding_signature` row in `pgmcp_metadata`.
/// Defaults to `minilm-l6-v2`; the operator flips it to `bge-m3-v1` once the
/// embedding-migration cron has drained the backlog.
async fn ensure_active_embedding_signature(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO pgmcp_metadata (key, value)
         VALUES ('active_embedding_signature', 'minilm-l6-v2')
         ON CONFLICT (key) DO NOTHING",
    )
    .execute(pool)
    .await?;
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

    // Also gate on the legacy column's presence: post-`embed-cutover
    // --drop-legacy` the `embedding` column is gone, so `CREATE INDEX … (embedding
    // …)` would throw. embedding_v2 has its own index helper
    // (ensure_memory_v2_hnsw_index), so dropping this legacy index entirely is fine.
    if needs_rebuild && column_exists(pool, "file_chunks", "embedding").await? {
        // Drop old index if it exists
        sqlx::query("DROP INDEX IF EXISTS idx_chunks_embedding")
            .execute(pool)
            .await?;

        // Create new HNSW index with configured parameters
        let create_sql = format!(
            "CREATE INDEX idx_chunks_embedding ON file_chunks USING hnsw (embedding vector_cosine_ops) \
             WITH (m = {}, ef_construction = {})",
            config.hnsw_m, config.hnsw_ef_construction
        );
        // Ignore error if table is empty (index creation on empty table is fast)
        build_hnsw_index(pool, config, &create_sql).await?;

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

    if needs_rebuild && column_exists(pool, "git_commit_chunks", "embedding").await? {
        sqlx::query("DROP INDEX IF EXISTS idx_git_commit_chunks_embedding")
            .execute(pool)
            .await?;

        let create_sql = format!(
            "CREATE INDEX idx_git_commit_chunks_embedding ON git_commit_chunks \
             USING hnsw (embedding vector_cosine_ops) WITH (m = {}, ef_construction = {})",
            config.hnsw_m, config.hnsw_ef_construction
        );
        build_hnsw_index(pool, config, &create_sql).await?;

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

    if needs_rebuild && column_exists(pool, "software_pattern_chunks", "embedding").await? {
        sqlx::query("DROP INDEX IF EXISTS idx_software_pattern_chunks_embedding")
            .execute(pool)
            .await?;

        let create_sql = format!(
            "CREATE INDEX idx_software_pattern_chunks_embedding ON software_pattern_chunks \
             USING hnsw (embedding vector_cosine_ops) WITH (m = {}, ef_construction = {})",
            config.hnsw_m, config.hnsw_ef_construction
        );
        build_hnsw_index(pool, config, &create_sql).await?;

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

    if stored.as_deref() != Some(&current_params)
        && column_exists(pool, "session_prompts", "embedding").await?
    {
        sqlx::query("DROP INDEX IF EXISTS idx_session_prompts_embedding")
            .execute(pool)
            .await?;

        let create_sql = format!(
            "CREATE INDEX idx_session_prompts_embedding ON session_prompts \
             USING hnsw (embedding vector_cosine_ops) WITH (m = {}, ef_construction = {})",
            config.hnsw_m, config.hnsw_ef_construction
        );
        build_hnsw_index(pool, config, &create_sql).await?;

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
