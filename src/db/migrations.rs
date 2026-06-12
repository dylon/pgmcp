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

use crate::parsing::type_tags::vocabulary::{SEED_EFFECTS, SEED_TYPE_TAGS, TagDef};

mod schema_introspect;
mod v10_fk_index_hardening;
mod v11_nudge_emissions;
mod v12_bug_tracker;
mod v13_fts_stored_tsv;
mod v14_resolution_kind_vocab;
mod v15_symbol_effect_history;
mod v16_assignee;
mod v17_git_links;
mod v18_digest_emissions;
mod v19_data_tables;
mod v20_unresolved_ref_index;
mod v21_sync_ops;
mod v22_concurrency_findings;
mod v23_ontology;
mod v24_extracted_content_hash;
mod v25_client_tracking;
mod v26_client_file_events;
mod v27_agent_social;
mod v28_project_deps_gitstate;
mod v29_coordination;
mod v2_shadow_asr;
mod v30_chunk_delete_index_hardening;
mod v31_graph_embeddings;
mod v32_toolbox_catalog;
mod v33_toolbox_domain_security;
mod v34_external_scanner_findings;
mod v3_cross_language_signatures;
mod v4_work_items;
mod v5_work_items_collab;
mod v6_unified_graph;
mod v7_cge_orphan_cleanup;
mod v8_csm_protocols;
mod v9_quality_report_history;
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

/// HNSW index on `work_items.embedding` (1024-d BGE-M3) for semantic backlog
/// search. Drop + rebuild only when the configured (m, ef_construction) change
/// — the `ensure_experiment_hnsw_index` idiom. Called unconditionally after the
/// v4 step so the table exists on both fresh and existing installs.
async fn ensure_work_items_hnsw_index(
    pool: &PgPool,
    config: &VectorConfig,
) -> Result<(), sqlx::Error> {
    let current_params = format!(
        "m={},ef_construction={}",
        config.hnsw_m, config.hnsw_ef_construction
    );
    let meta_key = "work_items_hnsw_params";
    let stored: Option<String> =
        sqlx::query_scalar("SELECT value FROM pgmcp_metadata WHERE key = $1")
            .bind(meta_key)
            .fetch_optional(pool)
            .await?;
    if stored.as_deref() != Some(&current_params) {
        sqlx::query("DROP INDEX IF EXISTS idx_work_items_embedding")
            .execute(pool)
            .await?;
        build_hnsw_index(
            pool,
            config,
            &format!(
                "CREATE INDEX IF NOT EXISTS idx_work_items_embedding ON work_items \
                 USING hnsw (embedding vector_cosine_ops) WITH (m = {}, ef_construction = {})",
                config.hnsw_m, config.hnsw_ef_construction
            ),
        )
        .await?;
        sqlx::query(
            "INSERT INTO pgmcp_metadata (key, value) VALUES ($1, $2)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(meta_key)
        .bind(&current_params)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// HNSW index on `data_tables.embedding` (1024-d BGE-M3) for semantic table
/// discovery (`data_table_search`). Drop + rebuild only when the configured
/// (m, ef_construction) change — the same params-tracking discipline as
/// `ensure_work_items_hnsw_index`. Called unconditionally after the v19 step so
/// the table exists on both fresh and existing installs.
async fn ensure_data_tables_hnsw_index(
    pool: &PgPool,
    config: &VectorConfig,
) -> Result<(), sqlx::Error> {
    let current_params = format!(
        "m={},ef_construction={}",
        config.hnsw_m, config.hnsw_ef_construction
    );
    let meta_key = "data_tables_hnsw_params";
    let stored: Option<String> =
        sqlx::query_scalar("SELECT value FROM pgmcp_metadata WHERE key = $1")
            .bind(meta_key)
            .fetch_optional(pool)
            .await?;
    if stored.as_deref() != Some(&current_params) {
        sqlx::query("DROP INDEX IF EXISTS idx_data_tables_embedding")
            .execute(pool)
            .await?;
        build_hnsw_index(
            pool,
            config,
            &format!(
                "CREATE INDEX IF NOT EXISTS idx_data_tables_embedding ON data_tables \
                 USING hnsw (embedding vector_cosine_ops) WITH (m = {}, ef_construction = {})",
                config.hnsw_m, config.hnsw_ef_construction
            ),
        )
        .await?;
        sqlx::query(
            "INSERT INTO pgmcp_metadata (key, value) VALUES ($1, $2)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(meta_key)
        .bind(&current_params)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// HNSW index on a 1024-d `embedding` column for a table that gains one in v31
/// (`agent_messages` / `a2a_messages` / `memory_entities` /
/// `coordination_requests`). Same params-tracked drop+rebuild discipline as
/// `ensure_work_items_hnsw_index`, but generic over the table name and guarded by
/// `column_exists` so a partial install where v31 has not yet added the column
/// simply no-ops. `table` is always a compile-time literal from the call sites
/// (no untrusted interpolation). The matview's own HNSW already covers unified
/// search; these per-base-table indexes serve future direct semantic queries.
async fn ensure_v31_embedding_hnsw_index(
    pool: &PgPool,
    config: &VectorConfig,
    table: &str,
) -> Result<(), sqlx::Error> {
    if !column_exists(pool, table, "embedding").await? {
        return Ok(());
    }
    let current_params = format!(
        "m={},ef_construction={}",
        config.hnsw_m, config.hnsw_ef_construction
    );
    let meta_key = format!("{table}_hnsw_params");
    let index_name = format!("idx_{table}_embedding");
    let stored: Option<String> =
        sqlx::query_scalar("SELECT value FROM pgmcp_metadata WHERE key = $1")
            .bind(&meta_key)
            .fetch_optional(pool)
            .await?;
    if stored.as_deref() != Some(&current_params) {
        sqlx::query(&format!("DROP INDEX IF EXISTS {index_name}"))
            .execute(pool)
            .await?;
        build_hnsw_index(
            pool,
            config,
            &format!(
                "CREATE INDEX IF NOT EXISTS {index_name} ON {table} \
                 USING hnsw (embedding vector_cosine_ops) WITH (m = {}, ef_construction = {})",
                config.hnsw_m, config.hnsw_ef_construction
            ),
        )
        .await?;
        sqlx::query(
            "INSERT INTO pgmcp_metadata (key, value) VALUES ($1, $2)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(&meta_key)
        .bind(&current_params)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Tracker ↔ experiment bridge (Phase 10). A `kind='experiment'` work_item is a
/// lightweight tracking handle; the rich hypotheses/runs/samples/results stay
/// in the experiment tables. This join table links the two so an experiment
/// gains the tracker's priority/tags/progress/roll-up/claiming, and the
/// experiment's frozen-criterion statistical verdict can post trusted
/// (`source='experiment'`) verification evidence back to the work_item.
///
/// Guarded by a `to_regclass` preflight: created only when BOTH `work_items`
/// (tracker v4) and `experiments` (the sibling subsystem) exist, so a partial
/// install of either side cannot break migrations. Idempotent.
async fn ensure_work_item_experiment_bridge(pool: &PgPool) -> Result<(), sqlx::Error> {
    let has_both: bool = sqlx::query_scalar(
        "SELECT to_regclass('public.work_items') IS NOT NULL
            AND to_regclass('public.experiments') IS NOT NULL",
    )
    .fetch_one(pool)
    .await?;
    if !has_both {
        return Ok(());
    }
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS work_item_experiment (
            work_item_id    BIGINT NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
            experiment_id   BIGINT NOT NULL REFERENCES experiments(id) ON DELETE CASCADE,
            hypothesis_id   BIGINT REFERENCES experiment_hypotheses(id) ON DELETE SET NULL,
            experiment_slug TEXT NOT NULL,
            created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            PRIMARY KEY (work_item_id, experiment_id)
        )",
    )
    .execute(pool)
    .await?;
    // Reverse lookup: all work_items tracking a given experiment (the sync path).
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_wie_experiment ON work_item_experiment(experiment_id)",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Scientific-experiment subsystem tables. Idempotent (`CREATE TABLE IF NOT
/// EXISTS`), created after `ensure_memory_phase2_tables` so the FK targets
/// (`projects`, `indexed_files`, `file_chunks`, `code_topics`,
/// `memory_observations`) all exist. The structured experiment record is the
/// source of truth that renders the committed `docs/scientific-ledger/*.md`
/// ledgers; see `docs/experiments/` and the plan
/// `~/.claude/plans/plan-how-to-effectively-drifting-fox.md`.
///
/// Shape follows MLflow (Experiment → Run → samples/results/artifacts) with a
/// pre-registered, frozen acceptance criterion + a statistical decision that
/// MLflow lacks. New tables are 1024d-direct (`embedding vector(1024)`, the
/// `durable_mandates` model) — the embedding-migration cron backfills NULLs
/// and `experiment_open`/`experiment_decide` embed synchronously on write.
async fn ensure_experiment_tables(pool: &PgPool) -> Result<(), sqlx::Error> {
    // ENUMs (idempotent pg_type probe, matching `ensure_memory_phase2_tables`).
    let enum_stmts = [
        (
            "experiment_kind",
            "CREATE TYPE experiment_kind AS ENUM ('optimization','feature_refactor','feature_addition','bugfix','investigation','other')",
        ),
        (
            "experiment_status",
            "CREATE TYPE experiment_status AS ENUM ('open','measuring','decided','abandoned','superseded')",
        ),
        (
            "hypothesis_verdict",
            "CREATE TYPE hypothesis_verdict AS ENUM ('pending','accepted','rejected','inconclusive')",
        ),
        (
            "experiment_arm_kind",
            "CREATE TYPE experiment_arm_kind AS ENUM ('control','treatment','baseline')",
        ),
        (
            "effect_direction",
            "CREATE TYPE effect_direction AS ENUM ('increase','decrease','either','none')",
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

    // Root experiment: the observation/question + kind + provenance, with a
    // bi-temporal supersession chain (a re-run can obsolete an earlier one).
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS experiments (
            id                  BIGSERIAL PRIMARY KEY,
            slug                TEXT NOT NULL,
            title               TEXT NOT NULL,
            question            TEXT NOT NULL,
            context             TEXT,
            kind                experiment_kind NOT NULL DEFAULT 'other',
            project_id          INTEGER REFERENCES projects(id) ON DELETE CASCADE,
            status              experiment_status NOT NULL DEFAULT 'open',
            hardware            JSONB NOT NULL DEFAULT '{}'::jsonb,
            git_ref             TEXT,
            plan_ref            TEXT,
            correction          TEXT NOT NULL DEFAULT 'benjamini_hochberg',
            embedding           vector(1024),
            embedding_signature TEXT NOT NULL DEFAULT 'bge-m3-v1',
            observation_id      BIGINT REFERENCES memory_observations(id) ON DELETE SET NULL,
            decided_by          TEXT,
            created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            valid_from          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            valid_to            TIMESTAMPTZ,
            superseded_by       BIGINT REFERENCES experiments(id),
            UNIQUE (slug, valid_from)
        )",
    )
    .execute(pool)
    .await?;
    let exp_indexes = [
        "CREATE INDEX IF NOT EXISTS idx_experiments_project ON experiments (project_id) WHERE valid_to IS NULL",
        "CREATE INDEX IF NOT EXISTS idx_experiments_status  ON experiments (status)     WHERE valid_to IS NULL",
        "CREATE INDEX IF NOT EXISTS idx_experiments_kind    ON experiments (kind)       WHERE valid_to IS NULL",
        "CREATE INDEX IF NOT EXISTS idx_experiments_slug    ON experiments (slug)       WHERE valid_to IS NULL",
        "CREATE INDEX IF NOT EXISTS idx_experiments_fts ON experiments USING gin \
         (to_tsvector('english', coalesce(title,'') || ' ' || coalesce(question,'') || ' ' || coalesce(context,'')))",
    ];
    for stmt in exp_indexes {
        sqlx::query(stmt).execute(pool).await?;
    }

    // Anchor an experiment to the code it concerns (mirrors memory_code_anchor).
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS experiment_code_anchor (
            id            BIGSERIAL PRIMARY KEY,
            experiment_id BIGINT NOT NULL REFERENCES experiments(id) ON DELETE CASCADE,
            file_id       BIGINT REFERENCES indexed_files(id) ON DELETE CASCADE,
            chunk_id      BIGINT REFERENCES file_chunks(id) ON DELETE CASCADE,
            topic_id      BIGINT REFERENCES code_topics(id) ON DELETE CASCADE,
            anchor_type   TEXT NOT NULL,
            created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            CHECK (file_id IS NOT NULL OR chunk_id IS NOT NULL OR topic_id IS NOT NULL)
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_experiment_code_anchor_exp ON experiment_code_anchor (experiment_id)",
    )
    .execute(pool)
    .await?;

    // Hypothesis with a PRE-REGISTERED, frozen acceptance criterion. The
    // criterion JSONB is opaque here; `crate::stats::acceptance` interprets
    // it. `criterion_locked_at` proves the criterion predates measurement
    // (anti-p-hacking, enforced in experiment_decide).
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS experiment_hypotheses (
            id                   BIGSERIAL PRIMARY KEY,
            experiment_id        BIGINT NOT NULL REFERENCES experiments(id) ON DELETE CASCADE,
            statement            TEXT NOT NULL,
            primary_metric       TEXT NOT NULL,
            unit                 TEXT,
            predicted_direction  effect_direction NOT NULL DEFAULT 'either',
            acceptance_criterion JSONB NOT NULL,
            criterion_locked_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            planned_n            INTEGER,
            verdict              hypothesis_verdict NOT NULL DEFAULT 'pending',
            embedding            vector(1024),
            embedding_signature  TEXT NOT NULL DEFAULT 'bge-m3-v1',
            created_at           TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            valid_from           TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            valid_to             TIMESTAMPTZ,
            superseded_by        BIGINT REFERENCES experiment_hypotheses(id)
        )",
    )
    .execute(pool)
    .await?;
    let hyp_indexes = [
        "CREATE INDEX IF NOT EXISTS idx_experiment_hypotheses_exp     ON experiment_hypotheses (experiment_id)",
        "CREATE INDEX IF NOT EXISTS idx_experiment_hypotheses_verdict ON experiment_hypotheses (verdict)",
        "CREATE INDEX IF NOT EXISTS idx_experiment_hypotheses_fts ON experiment_hypotheses USING gin \
         (to_tsvector('english', coalesce(statement,'')))",
    ];
    for stmt in hyp_indexes {
        sqlx::query(stmt).execute(pool).await?;
    }

    // One arm execution / metric collection. The agent reports command_spec
    // + host_meta (hardware/governor/pinning) for reproducibility.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS experiment_runs (
            id            UUID PRIMARY KEY,
            experiment_id BIGINT NOT NULL REFERENCES experiments(id) ON DELETE CASCADE,
            hypothesis_id BIGINT REFERENCES experiment_hypotheses(id) ON DELETE SET NULL,
            arm_label     TEXT NOT NULL,
            arm_kind      experiment_arm_kind NOT NULL,
            command_spec  JSONB NOT NULL DEFAULT '{}'::jsonb,
            run_plan      JSONB NOT NULL DEFAULT '{}'::jsonb,
            host_meta     JSONB NOT NULL DEFAULT '{}'::jsonb,
            git_ref       TEXT,
            runner        TEXT,
            seed          BIGINT NOT NULL DEFAULT 0,
            status        TEXT NOT NULL DEFAULT 'pending',
            error         TEXT,
            started_at    TIMESTAMPTZ,
            finished_at   TIMESTAMPTZ,
            created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            UNIQUE (experiment_id, hypothesis_id, arm_label)
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_experiment_runs_exp ON experiment_runs (experiment_id)",
    )
    .execute(pool)
    .await?;

    // Raw per-replicate samples (row-per-replicate so warm-up is flaggable
    // and steady-state filtering is an index predicate — Kalibera-Jones). For
    // deterministic distribution-valued metrics (per-file complexity), one
    // row per measured unit, keyed by `unit_key` for the paired Wilcoxon test.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS experiment_samples (
            id              BIGSERIAL PRIMARY KEY,
            run_id          UUID NOT NULL REFERENCES experiment_runs(id) ON DELETE CASCADE,
            arm             TEXT NOT NULL,
            metric_name     TEXT NOT NULL,
            replicate_index INTEGER NOT NULL,
            value           DOUBLE PRECISION NOT NULL,
            unit_key        TEXT,
            is_warmup       BOOLEAN NOT NULL DEFAULT FALSE,
            recorded_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_experiment_samples_run ON experiment_samples \
         (run_id, metric_name, arm) WHERE NOT is_warmup",
    )
    .execute(pool)
    .await?;

    // The statistical decision (verdict + full TestResult evidence).
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS experiment_results (
            id                  BIGSERIAL PRIMARY KEY,
            experiment_id       BIGINT NOT NULL REFERENCES experiments(id) ON DELETE CASCADE,
            hypothesis_id       BIGINT NOT NULL REFERENCES experiment_hypotheses(id) ON DELETE CASCADE,
            test_type           TEXT NOT NULL,
            metric_name         TEXT NOT NULL,
            control_run_id      UUID REFERENCES experiment_runs(id) ON DELETE SET NULL,
            treatment_run_id    UUID REFERENCES experiment_runs(id) ON DELETE SET NULL,
            statistic           DOUBLE PRECISION,
            df                  DOUBLE PRECISION,
            p_value             DOUBLE PRECISION,
            effect_size         DOUBLE PRECISION,
            effect_size_kind    TEXT,
            ci_low              DOUBLE PRECISION,
            ci_high             DOUBLE PRECISION,
            ci_level            DOUBLE PRECISION DEFAULT 0.95,
            verdict             hypothesis_verdict NOT NULL,
            accepted            BOOLEAN NOT NULL,
            correction          TEXT,
            criterion_snapshot  JSONB NOT NULL DEFAULT '{}'::jsonb,
            test_result         JSONB NOT NULL DEFAULT '{}'::jsonb,
            rationale           TEXT,
            decided_by          TEXT,
            embedding           vector(1024),
            embedding_signature TEXT NOT NULL DEFAULT 'bge-m3-v1',
            observation_id      BIGINT REFERENCES memory_observations(id) ON DELETE SET NULL,
            created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await?;
    let res_indexes = [
        "CREATE INDEX IF NOT EXISTS idx_experiment_results_hyp ON experiment_results (hypothesis_id)",
        "CREATE INDEX IF NOT EXISTS idx_experiment_results_exp ON experiment_results (experiment_id)",
        "CREATE INDEX IF NOT EXISTS idx_experiment_results_fts ON experiment_results USING gin \
         (to_tsvector('english', coalesce(rationale,'')))",
    ];
    for stmt in res_indexes {
        sqlx::query(stmt).execute(pool).await?;
    }

    // Ad-hoc profiling/benchmark/debug capture: tied to an experiment
    // (experiment_id set) or free-standing (`experiment_id` NULL, project_id
    // only) for the low-ceremony "I profiled this, remember it" path.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS experiment_artifacts (
            id                  BIGSERIAL PRIMARY KEY,
            experiment_id       BIGINT REFERENCES experiments(id) ON DELETE CASCADE,
            project_id          INTEGER REFERENCES projects(id) ON DELETE CASCADE,
            kind                TEXT NOT NULL,
            tool                TEXT,
            label               TEXT,
            content             TEXT,
            content_sha256      CHAR(64),
            metrics             JSONB NOT NULL DEFAULT '{}'::jsonb,
            file_id             BIGINT REFERENCES indexed_files(id) ON DELETE SET NULL,
            embedding           vector(1024),
            embedding_signature TEXT NOT NULL DEFAULT 'bge-m3-v1',
            git_ref             TEXT,
            created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await?;
    let art_indexes = [
        "CREATE INDEX IF NOT EXISTS idx_experiment_artifacts_exp  ON experiment_artifacts (experiment_id) WHERE experiment_id IS NOT NULL",
        "CREATE INDEX IF NOT EXISTS idx_experiment_artifacts_proj ON experiment_artifacts (project_id, kind)",
        "CREATE INDEX IF NOT EXISTS idx_experiment_artifacts_fts ON experiment_artifacts USING gin \
         (to_tsvector('english', coalesce(content,'')))",
    ];
    for stmt in art_indexes {
        sqlx::query(stmt).execute(pool).await?;
    }

    Ok(())
}

/// HNSW indexes for the experiment subsystem's four embedding columns, with
/// the same params-tracking rebuild discipline as
/// `ensure_memory_phase2_hnsw_index` (drop + rebuild only when
/// `[vector] m / ef_construction` change). Built on the (initially empty)
/// `vector(1024)` columns; pgvector maintains them incrementally on insert.
async fn ensure_experiment_hnsw_index(
    pool: &PgPool,
    config: &VectorConfig,
) -> Result<(), sqlx::Error> {
    let current_params = format!(
        "m={},ef_construction={}",
        config.hnsw_m, config.hnsw_ef_construction
    );
    // (metadata key, index name, table, embedding column)
    let specs = [
        (
            "experiments_hnsw_params",
            "idx_experiments_embedding",
            "experiments",
        ),
        (
            "experiment_hypotheses_hnsw_params",
            "idx_experiment_hypotheses_embedding",
            "experiment_hypotheses",
        ),
        (
            "experiment_results_hnsw_params",
            "idx_experiment_results_embedding",
            "experiment_results",
        ),
        (
            "experiment_artifacts_hnsw_params",
            "idx_experiment_artifacts_embedding",
            "experiment_artifacts",
        ),
    ];
    for (meta_key, index_name, table) in specs {
        let stored: Option<String> =
            sqlx::query_scalar("SELECT value FROM pgmcp_metadata WHERE key = $1")
                .bind(meta_key)
                .fetch_optional(pool)
                .await?;
        if stored.as_deref() != Some(&current_params) {
            sqlx::query(&format!("DROP INDEX IF EXISTS {index_name}"))
                .execute(pool)
                .await?;
            let create_sql = format!(
                "CREATE INDEX {index_name} ON {table} \
                 USING hnsw (embedding vector_cosine_ops) WITH (m = {}, ef_construction = {})",
                config.hnsw_m, config.hnsw_ef_construction
            );
            build_hnsw_index(pool, config, &create_sql).await?;
            sqlx::query(
                "INSERT INTO pgmcp_metadata (key, value) VALUES ($1, $2)
                 ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
            )
            .bind(meta_key)
            .bind(&current_params)
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

// This migration-lock-retry cluster is reached only from the daemon startup
// path (`cli::daemon`, via `run_migrations_with_lock_retry` at startup step 1),
// never from a `#[cfg(test)]` test. In the `bin pgmcp test` target the harness
// replaces `main`, so code reachable only through `main` is flagged `dead_code`
// there even though it is live in the running daemon — hence the targeted
// allows (the lib build keeps these reachable via the `pub` API).
/// Number of times [`run_migrations_with_lock_retry`] retries on a
/// `lock_timeout` before giving up.
#[allow(dead_code)]
const MIGRATION_LOCK_RETRIES: u32 = 6;
/// Backoff between migration retries. Roughly one `lock_timeout` window, so each
/// retry re-waits about as long as the failed lock attempt did.
#[allow(dead_code)]
const MIGRATION_LOCK_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_secs(5);

/// True if `e` is PostgreSQL `lock_not_available` (SQLSTATE 55P03) — a statement
/// cancelled by `lock_timeout` while waiting for a lock.
#[allow(dead_code)] // live only in the daemon path; see the cluster note above
fn is_lock_timeout(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.code().as_deref() == Some("55P03"))
}

/// [`run_migrations`], retried on transient lock contention.
///
/// Startup migrations take ACCESS EXCLUSIVE locks (table / column DDL). If a
/// previous daemon instance was killed mid-query, its orphaned backend can still
/// hold ACCESS SHARE on a table our DDL needs — so the first attempt may hit
/// `lock_timeout` (SQLSTATE 55P03). Rather than abort startup, we retry: the
/// orphan is reaped by `client_connection_check_interval` (or finishes), the
/// lock frees, and a later attempt succeeds. `run_migrations` is idempotent
/// (IF NOT EXISTS + version gates + nullability / constraint guards), so
/// re-running from the top is safe. Non-lock errors propagate immediately.
#[allow(dead_code)] // live only in the daemon path; see the cluster note above
pub async fn run_migrations_with_lock_retry(
    pool: &PgPool,
    vector_config: &VectorConfig,
) -> Result<(), sqlx::Error> {
    let mut attempt = 0u32;
    loop {
        match run_migrations(pool, vector_config).await {
            Ok(()) => return Ok(()),
            Err(e) if is_lock_timeout(&e) && attempt < MIGRATION_LOCK_RETRIES => {
                attempt += 1;
                tracing::warn!(
                    attempt,
                    max = MIGRATION_LOCK_RETRIES,
                    backoff_secs = MIGRATION_LOCK_RETRY_BACKOFF.as_secs(),
                    error = %e,
                    "startup migrations hit lock contention (lock_timeout); retrying after backoff"
                );
                tokio::time::sleep(MIGRATION_LOCK_RETRY_BACKOFF).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Apply one version-gated migration step with uniform progress logging.
///
/// Emits an INFO `"starting migration step"` line *before* `body` runs and an
/// INFO `"migration step applied"` line carrying `elapsed_ms` after the version
/// is recorded. A long step — e.g. v13's `GENERATED ALWAYS … STORED` column add
/// that rewrites `file_chunks` and can run for tens of minutes — is then legible
/// as *in progress* in the log instead of looking like a hung daemon (the
/// failure mode investigated in
/// `~/.claude/plans/pgmcp-has-not-logged-structured-sprout.md`). Steps already
/// at or past `version` are skipped silently, exactly as the previous
/// `if !version_applied { … }` blocks did.
async fn apply_step<F, Fut>(
    pool: &PgPool,
    version: i32,
    name: &str,
    body: F,
) -> Result<(), sqlx::Error>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<(), sqlx::Error>>,
{
    if version_applied(pool, version).await? {
        return Ok(());
    }
    info!(
        version,
        name, "starting migration step (large-table rewrites can take minutes)"
    );
    let started = std::time::Instant::now();
    body().await?;
    record_version(pool, version, name).await?;
    info!(
        version,
        name,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "migration step applied"
    );
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
    if !initial_schema_done {
        info!(
            "starting initial schema bootstrap (first run creates every base table \
             and index; on a large pre-existing corpus this can take several minutes)"
        );
    }
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
            UNIQUE (file_id, chunk_index)
        )",
    )
    .execute(pool)
    .await?;

    // Migration: allow content_hash to be NULL (deferred commit for resume-safety).
    // Existing databases may have NOT NULL; this is metadata-only in PostgreSQL.
    //
    // Guarded on current nullability. `ALTER … DROP NOT NULL` has no `IF` form and
    // takes ACCESS EXCLUSIVE on `indexed_files`; this block re-runs on every boot,
    // so issuing it unconditionally meant a restart landing while a long analytic
    // query (e.g. semantic-edges) held ACCESS SHARE would block on the lock and
    // abort startup at `lock_timeout`. Skip the no-op ALTER on the common path.
    if !column_is_nullable(pool, "indexed_files", "content_hash").await? {
        sqlx::query("ALTER TABLE indexed_files ALTER COLUMN content_hash DROP NOT NULL")
            .execute(pool)
            .await?;
    }

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
        // The per-chunk full-text index lives on the stored
        // `file_chunks.content_tsv` generated column and is created by the v13
        // migration (`idx_file_chunks_content_tsv`, fastupdate=off) — NOT here.
        // That column is added later in the boot by the gated v13 step, so it
        // cannot be indexed from this unconditional initial-schema block; and
        // creating the legacy `idx_file_chunks_fts` here would resurrect it
        // every boot after v13 drops it. Chunk content is bounded above by
        // TSVECTOR_SAFE_CHUNK_BYTES (900 KiB) so every chunk's tsvector fits
        // comfortably under the 1 MiB cap.
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
            -- CASCADE, NOT SET NULL: target_file_id is a member of the unique
            -- index idx_cge_unique (below) via COALESCE(target_file_id, -1).
            -- Under SET NULL, deleting a referenced file nulled this column on
            -- surviving edges, collapsing their key to (source, -1, type, raw)
            -- and colliding with idx_cge_unique — which failed the parent
            -- DELETE. An edge whose target file is gone is meaningless; the
            -- graph-analysis cron rebuilds resolved imports as unresolved on
            -- its next pass. The re-tighten DO block further below repairs
            -- pre-existing installs (CREATE TABLE IF NOT EXISTS won't alter
            -- them). See docs/scientific-ledger/idx-cge-unique-set-null-collision-2026-05-27.md.
            target_file_id BIGINT REFERENCES indexed_files(id) ON DELETE CASCADE,
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

    // Idempotent CHECK install. `ensure_named_constraint` skips the
    // ACCESS-EXCLUSIVE DROP+ADD (which revalidates every `code_graph_edges` row)
    // when the constraint is already installed with this exact definition, so the
    // per-boot re-run is a single catalog read. See its doc for the restart-time
    // lock-collision rationale.
    ensure_named_constraint(
        pool,
        "code_graph_edges",
        "cge_call_needs_source_symbol",
        "CHECK (edge_type <> 'call' OR source_symbol_id IS NOT NULL)",
    )
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

    // Re-tighten the target_file_id FK from ON DELETE SET NULL to ON DELETE
    // CASCADE. Unlike source_symbol_id above (whose SET NULL conflicted with a
    // CHECK), target_file_id is broken because it is a MEMBER of the unique
    // index idx_cge_unique via COALESCE(target_file_id, -1::BIGINT). When an
    // indexed_files row is deleted, the cascade NULLs target_file_id on every
    // edge that pointed at it; after COALESCE the surviving row's key collapses
    // to (source_file_id, -1, edge_type, COALESCE(target_raw,'')). If another
    // edge from the same source already has target_file_id IS NULL with the
    // same (edge_type, target_raw) — which accumulates as referenced files are
    // deleted over time — the SET NULL update COLLIDES with idx_cge_unique and
    // fails the parent DELETE. Observed in production as "Failed to delete file
    // from index … duplicate key value violates unique constraint
    // idx_cge_unique" from pgmcp::embed::pool, notably on rotating
    // ~/.claude/sessions/*.json files. See
    // docs/scientific-ledger/idx-cge-unique-set-null-collision-2026-05-27.md.
    //
    // CASCADE is the correct response: an edge whose target file no longer
    // exists is meaningless and is removed; the graph-analysis cron rebuilds a
    // still-valid import as unresolved (target_file_id NULL, target_raw kept)
    // on its next pass via ON CONFLICT DO UPDATE, so nothing is permanently
    // lost. (target_symbol_id keeps SET NULL — it is NOT in any unique index,
    // so nulling it never collides; migration step 7 then removes the orphan
    // NULL-target rows the old SET NULL already left behind.)
    //
    // Idempotent DO block, identical idiom to the source_symbol_id re-tighten
    // above: look up the FK name + confdeltype dynamically from pg_constraint
    // and rewrite only when not already CASCADE ('c'), so re-running against an
    // already-fixed DB is a no-op. confdeltype per Postgres docs:
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
               AND a.attname = 'target_file_id'
               AND c.contype = 'f'
             LIMIT 1;
            IF con_name IS NOT NULL AND con_deltype <> 'c' THEN
                EXECUTE format('ALTER TABLE code_graph_edges DROP CONSTRAINT %I', con_name);
                ALTER TABLE code_graph_edges
                    ADD CONSTRAINT code_graph_edges_target_file_id_fkey
                    FOREIGN KEY (target_file_id)
                    REFERENCES indexed_files(id)
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

    ensure_named_constraint(
        pool,
        "software_patterns",
        "software_patterns_kind_check",
        "CHECK (kind IN ('pattern', 'anti_pattern', 'principle', 'code_smell'))",
    )
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

    // Idempotent CHECK-constraint installs (see `ensure_named_constraint`): these
    // re-run every boot, so each skips its ACCESS-EXCLUSIVE DROP+ADD unless the
    // definition actually changed (e.g. a new mandate polarity is added).
    ensure_named_constraint(
        pool,
        "session_mandates",
        "session_mandates_polarity_check",
        "CHECK (polarity IN ('always','never','prefer','avoid','remember','from_now_on',\
         'correction','permission','constraint','mandate','process_rule','project_rule'))",
    )
    .await?;
    ensure_named_constraint(
        pool,
        "session_mandates",
        "session_mandates_status_check",
        "CHECK (status IN ('active','superseded','retired','promoted'))",
    )
    .await?;
    ensure_named_constraint(
        pool,
        "durable_mandates",
        "durable_mandates_scope_check",
        "CHECK (scope IN ('project','workspace'))",
    )
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
    // incrementally. Post-ADR-005 (1024-only) the legacy `embedding` column is
    // dropped below and `embedding_v2` is the sole vector column;
    // `active_embedding_signature` is pinned to `bge-m3-v1`.
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

    // Scientific-experiment subsystem (depends on the tables above for FKs).
    ensure_experiment_tables(pool).await?;
    ensure_experiment_hnsw_index(pool, vector_config).await?;

    // Tracker ↔ experiment bridge (Phase 10). Created LATE and guarded by a
    // to_regclass preflight because `experiments` is itself an inline ensure_*
    // (above) and the tracker's `work_items` is the numbered v4 step — this
    // keeps migration order-independent and resilient to either subsystem being
    // absent in a partial install.
    ensure_work_item_experiment_bridge(pool).await?;

    // (memory_unified_views is built LAST — after all numbered migration steps
    // — because its node/edge arms reference columns/tables added by v6
    // (work_items.observation_id, experiment_relations, memory_code_anchor
    // .symbol_id/.project_id). See the call after the v6 step below.)

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
    apply_step(
        pool,
        v2_shadow_asr::SHADOW_ASR_V1,
        v2_shadow_asr::SHADOW_ASR_V1_NAME,
        || v2_shadow_asr::apply(pool),
    )
    .await?;

    // ================================================================
    // Migration step 3 — cross_language_signatures_v1
    // Materialized cross-language clone table powering
    // `mcp__pgmcp__cross_language_api_equivalents` and downstream
    // similarity tools.
    // ================================================================
    apply_step(
        pool,
        v3_cross_language_signatures::CROSS_LANGUAGE_SIGNATURES_V1,
        v3_cross_language_signatures::CROSS_LANGUAGE_SIGNATURES_V1_NAME,
        || v3_cross_language_signatures::apply(pool),
    )
    .await?;

    // ================================================================
    // Migration step 4 — work_items_v1
    // Work-item / plan tracker: work_items spine + status-history audit,
    // tags, progress, plan_definitions + rules, acceptance_criteria +
    // verification_evidence, relations, code anchors, scope_negotiations.
    // Runs after `ensure_experiment_tables` (above) so the Phase-10
    // experiment bridge (v5) has its FK target. See
    // `src/db/migrations/v4_work_items.rs` and the plan
    // `~/.claude/plans/plan-mcp-support-for-moonlit-dongarra.md`.
    // ================================================================
    apply_step(
        pool,
        v4_work_items::WORK_ITEMS_V1,
        v4_work_items::WORK_ITEMS_V1_NAME,
        || v4_work_items::apply(pool),
    )
    .await?;

    // Re-apply the work_items kind/status/origin vocabulary CHECKs
    // UNCONDITIONALLY on every startup (idempotent DROP+ADD). The vocabulary is
    // built from the closed Rust enums (`crate::tracker::kind|status`), so a new
    // kind/status (e.g. adding `brainstorm`) becomes a constraint swap that
    // lands on existing installs too — not just fresh ones gated behind the v4
    // version flag. Guarded by a to_regclass preflight in case work_items is
    // somehow absent.
    if sqlx::query_scalar::<_, bool>("SELECT to_regclass('public.work_items') IS NOT NULL")
        .fetch_one(pool)
        .await?
    {
        v4_work_items::install_work_items_checks(pool).await?;
    }

    // ================================================================
    // Migration step 5 — work_items_collab_v1
    // A2A collaboration layer: claim/lease columns on work_items,
    // work_item_claims ledger, agent_presence, agent_identity view.
    // Runs after v4 (work_items) and the initial schema (a2a_agents).
    // ================================================================
    apply_step(
        pool,
        v5_work_items_collab::WORK_ITEMS_COLLAB_V1,
        v5_work_items_collab::WORK_ITEMS_COLLAB_V1_NAME,
        || v5_work_items_collab::apply(pool),
    )
    .await?;

    // ================================================================
    // Migration step 6 — unified_graph_v1
    // Unified knowledge-graph foundation: work_items.observation_id,
    // experiment_relations (inter-experiment DAG), memory_code_anchor
    // +symbol_id/+project_id (relaxed CHECK), and the 'auto_index'
    // memory_source value. The work_item_experiment bridge already exists
    // (ensure_work_item_experiment_bridge); Stage 2 wires it into the views.
    // See `src/db/migrations/v6_unified_graph.rs`.
    // ================================================================
    apply_step(
        pool,
        v6_unified_graph::UNIFIED_GRAPH_V1,
        v6_unified_graph::UNIFIED_GRAPH_V1_NAME,
        || v6_unified_graph::apply(pool),
    )
    .await?;

    // ================================================================
    // Migration step 7 — cge_orphan_cleanup_v1
    // One-time removal of code_graph_edges rows orphaned by the old
    // target_file_id ON DELETE SET NULL behavior (semantic / co-change
    // edges left with a NULL target and NULL target_raw). The FK itself is
    // re-tightened to CASCADE inline far above; this step deletes the rows
    // the old behavior already left behind. Runs after every table exists.
    // See src/db/migrations/v7_cge_orphan_cleanup.rs.
    // ================================================================
    apply_step(
        pool,
        v7_cge_orphan_cleanup::CGE_ORPHAN_CLEANUP_V1,
        v7_cge_orphan_cleanup::CGE_ORPHAN_CLEANUP_V1_NAME,
        || v7_cge_orphan_cleanup::apply(pool),
    )
    .await?;

    // ================================================================
    // Step 8: CSM / MPST coordination tables (ADR-009).
    // See src/db/migrations/v8_csm_protocols.rs.
    // ================================================================
    apply_step(
        pool,
        v8_csm_protocols::CSM_PROTOCOLS_V1,
        v8_csm_protocols::CSM_PROTOCOLS_V1_NAME,
        || v8_csm_protocols::apply(pool),
    )
    .await?;

    // ================================================================
    // Step 9: quality_report GPA history (trend strip).
    // See src/db/migrations/v9_quality_report_history.rs.
    // ================================================================
    apply_step(
        pool,
        v9_quality_report_history::QUALITY_REPORT_HISTORY_V1,
        v9_quality_report_history::QUALITY_REPORT_HISTORY_V1_NAME,
        || v9_quality_report_history::apply(pool),
    )
    .await?;

    // ================================================================
    // Step 10: FK child-column index hardening + memory_observations
    // source FK ON DELETE SET NULL. See src/db/migrations/v10_fk_index_hardening.rs.
    // All base tables it touches exist by now (created in the version-1 body).
    // ================================================================
    apply_step(
        pool,
        v10_fk_index_hardening::FK_INDEX_HARDENING_V1,
        v10_fk_index_hardening::FK_INDEX_HARDENING_V1_NAME,
        || v10_fk_index_hardening::apply(pool),
    )
    .await?;

    // Step 11: nudge_emissions (JIT adoption-nudge log + rate-limit source).
    // Registered before the unconditional ensure_* steps below.
    apply_step(
        pool,
        v11_nudge_emissions::NUDGE_EMISSIONS_V1,
        v11_nudge_emissions::NUDGE_EMISSIONS_V1_NAME,
        || v11_nudge_emissions::apply(pool),
    )
    .await?;

    // Step 12: bug_tracker (severity column + work_item_bug_details sidecar +
    // triage/confirmed lifecycle states). Gated; the unconditional
    // install_work_items_checks reconcile (above) already picked up the new
    // kind/status vocab on existing installs, and v12::apply installs the
    // severity CHECK once the column exists.
    apply_step(
        pool,
        v12_bug_tracker::BUG_TRACKER_V1,
        v12_bug_tracker::BUG_TRACKER_V1_NAME,
        || v12_bug_tracker::apply(pool),
    )
    .await?;

    apply_step(
        pool,
        v13_fts_stored_tsv::FTS_STORED_TSV_V1,
        v13_fts_stored_tsv::FTS_STORED_TSV_V1_NAME,
        || v13_fts_stored_tsv::apply(pool),
    )
    .await?;

    apply_step(
        pool,
        v14_resolution_kind_vocab::RESOLUTION_KIND_VOCAB_V1,
        v14_resolution_kind_vocab::RESOLUTION_KIND_VOCAB_V1_NAME,
        || v14_resolution_kind_vocab::apply(pool),
    )
    .await?;

    apply_step(
        pool,
        v15_symbol_effect_history::SYMBOL_EFFECT_HISTORY_V1,
        v15_symbol_effect_history::SYMBOL_EFFECT_HISTORY_V1_NAME,
        || v15_symbol_effect_history::apply(pool),
    )
    .await?;

    apply_step(
        pool,
        v16_assignee::WORK_ITEM_ASSIGNEE_V1,
        v16_assignee::WORK_ITEM_ASSIGNEE_V1_NAME,
        || v16_assignee::apply(pool),
    )
    .await?;

    apply_step(
        pool,
        v17_git_links::GIT_LINKS_V1,
        v17_git_links::GIT_LINKS_V1_NAME,
        || v17_git_links::apply(pool),
    )
    .await?;

    apply_step(
        pool,
        v18_digest_emissions::DIGEST_EMISSIONS_V1,
        v18_digest_emissions::DIGEST_EMISSIONS_V1_NAME,
        || v18_digest_emissions::apply(pool),
    )
    .await?;

    apply_step(
        pool,
        v19_data_tables::DATA_TABLES_V1,
        v19_data_tables::DATA_TABLES_V1_NAME,
        || v19_data_tables::apply(pool),
    )
    .await?;

    apply_step(
        pool,
        v20_unresolved_ref_index::UNRESOLVED_REF_INDEX_V1,
        v20_unresolved_ref_index::UNRESOLVED_REF_INDEX_V1_NAME,
        || v20_unresolved_ref_index::apply(pool),
    )
    .await?;

    apply_step(
        pool,
        v21_sync_ops::SYNC_OPS_V1,
        v21_sync_ops::SYNC_OPS_V1_NAME,
        || v21_sync_ops::apply(pool),
    )
    .await?;

    apply_step(
        pool,
        v22_concurrency_findings::CONCURRENCY_FINDINGS_V1,
        v22_concurrency_findings::CONCURRENCY_FINDINGS_V1_NAME,
        || v22_concurrency_findings::apply(pool),
    )
    .await?;

    apply_step(
        pool,
        v23_ontology::ONTOLOGY_V1,
        v23_ontology::ONTOLOGY_V1_NAME,
        || v23_ontology::apply(pool),
    )
    .await?;

    apply_step(
        pool,
        v24_extracted_content_hash::EXTRACTED_CONTENT_HASH_V1,
        v24_extracted_content_hash::EXTRACTED_CONTENT_HASH_V1_NAME,
        || v24_extracted_content_hash::apply(pool),
    )
    .await?;

    apply_step(
        pool,
        v25_client_tracking::CLIENT_TRACKING_V1,
        v25_client_tracking::CLIENT_TRACKING_V1_NAME,
        || v25_client_tracking::apply(pool),
    )
    .await?;

    apply_step(
        pool,
        v26_client_file_events::CLIENT_FILE_EVENTS_V1,
        v26_client_file_events::CLIENT_FILE_EVENTS_V1_NAME,
        || v26_client_file_events::apply(pool),
    )
    .await?;

    apply_step(
        pool,
        v27_agent_social::AGENT_SOCIAL_V1,
        v27_agent_social::AGENT_SOCIAL_V1_NAME,
        || v27_agent_social::apply(pool),
    )
    .await?;

    apply_step(
        pool,
        v28_project_deps_gitstate::PROJECT_DEPS_GITSTATE_V1,
        v28_project_deps_gitstate::PROJECT_DEPS_GITSTATE_V1_NAME,
        || v28_project_deps_gitstate::apply(pool),
    )
    .await?;

    apply_step(
        pool,
        v29_coordination::COORDINATION_V1,
        v29_coordination::COORDINATION_V1_NAME,
        || v29_coordination::apply(pool),
    )
    .await?;

    apply_step(
        pool,
        v30_chunk_delete_index_hardening::CHUNK_DELETE_INDEX_HARDENING_V1,
        v30_chunk_delete_index_hardening::CHUNK_DELETE_INDEX_HARDENING_V1_NAME,
        || v30_chunk_delete_index_hardening::apply(pool),
    )
    .await?;

    apply_step(
        pool,
        v31_graph_embeddings::GRAPH_EMBEDDINGS_V1,
        v31_graph_embeddings::GRAPH_EMBEDDINGS_V1_NAME,
        || v31_graph_embeddings::apply(pool),
    )
    .await?;

    apply_step(
        pool,
        v32_toolbox_catalog::TOOLBOX_CATALOG_V1,
        v32_toolbox_catalog::TOOLBOX_CATALOG_V1_NAME,
        || v32_toolbox_catalog::apply(pool),
    )
    .await?;

    apply_step(
        pool,
        v33_toolbox_domain_security::TOOLBOX_DOMAIN_SECURITY,
        v33_toolbox_domain_security::TOOLBOX_DOMAIN_SECURITY_NAME,
        || v33_toolbox_domain_security::apply(pool),
    )
    .await?;

    apply_step(
        pool,
        v34_external_scanner_findings::EXTERNAL_SCANNER_FINDINGS_V1,
        v34_external_scanner_findings::EXTERNAL_SCANNER_FINDINGS_V1_NAME,
        || v34_external_scanner_findings::apply(pool),
    )
    .await?;

    // Every-boot vocabulary-catalog reconcile (RC1 durable fix). Unconditional
    // and idempotent; closes the post-v2 catalog-drift gap that silently
    // FK-skipped symbol extraction for files carrying a newly-added effect.
    // MUST precede ensure_memory_unified_views (which sources effect_catalog /
    // type_tag_catalog as graph-node arms).
    reconcile_vocabulary_catalogs(pool).await?;

    // Build/refresh the work_items HNSW index (unconditional; the table exists
    // after the v4 step on fresh installs and already exists on upgrades).
    ensure_work_items_hnsw_index(pool, vector_config).await?;

    // Build/refresh the data_tables HNSW index (semantic table discovery via
    // `data_table_search`); same params-tracked rebuild discipline.
    ensure_data_tables_hnsw_index(pool, vector_config).await?;

    // Build/refresh HNSW indexes on the v31 graph-RAG embedding columns. Runs
    // after the v31 apply_step (above) so the columns exist; each call is
    // column_exists-guarded for partial installs. These rows also become
    // embedding-bearing arms in memory_unified_nodes below.
    for table in [
        "agent_messages",
        "a2a_messages",
        "memory_entities",
        "coordination_requests",
    ] {
        ensure_v31_embedding_hnsw_index(pool, vector_config, table).await?;
    }

    // Build/refresh the tool_cards HNSW index (semantic developer-tool discovery
    // via `toolbox_search`). The generic helper builds `idx_tool_cards_embedding`
    // on the 1024-d `embedding` column with the same params-tracked discipline,
    // column_exists-guarded; runs after the v32 apply_step so the column exists.
    ensure_v31_embedding_hnsw_index(pool, vector_config, "tool_cards").await?;

    // Stage 5c: trajectory-similarity edge store (must exist before the edges
    // view, which UNIONs it as the `evolves_like` arm).
    ensure_trajectory_similarities(pool).await?;

    // ================================================================
    // Memory-server Phase 6.3 + unified-graph: the heterogeneous node/edge
    // graph views. Built LAST so every node/edge arm's source table/column
    // exists — including the v6 additions (work_items.observation_id,
    // experiment_relations, memory_code_anchor.symbol_id/.project_id) and the
    // collaboration tables (work_item_claims, agent_presence, agent_identity).
    // Hash-gated (MEMORY_UNIFIED_VIEWS_HASH_KEY): rebuilds only when the SQL
    // consts change. See `docs/memory-server/05-schema.md` §12.3.
    // ================================================================
    ensure_memory_unified_views(pool, vector_config).await?;

    Ok(())
}

/// Reconcile the vocabulary catalogs (`effect_catalog`, `type_tag_catalog`)
/// with the Rust source-of-truth (`SEED_EFFECTS` / `SEED_TYPE_TAGS`).
/// Unconditional + idempotent — runs on every boot from `run_migrations`.
///
/// The v2 `shadow_asr` migration first-seeds the catalogs, but `apply_step`
/// gates it behind `version_applied`, so any effect / type tag *added* to the
/// vocabulary after v2 never reaches an already-migrated database. That gap
/// silently broke symbol extraction on 2026-06-01: the v21 concurrency effects
/// (`await_point`, `lock_acquire`, `lock_release`, `thread_spawn`,
/// `channel_select`) were missing from `effect_catalog`, so the
/// `symbol_effects_effect_fkey` FK rejected every symbol carrying one and the
/// entire file was skipped. This reconcile closes the gap permanently — it is
/// the catalog-superset half of the invariant ADR-003 anticipated.
///
/// Non-fatal by design: a residual gap is logged at `error!` (and caught by the
/// `vocabulary_catalog_parity` regression test), never panicked — a stale
/// catalog must not stop the daemon from starting.
async fn reconcile_vocabulary_catalogs(pool: &PgPool) -> Result<(), sqlx::Error> {
    seed_catalog(pool, "type_tag_catalog", SEED_TYPE_TAGS).await?;
    seed_catalog(pool, "effect_catalog", SEED_EFFECTS).await?;

    // Verify the catalog ⊇ vocabulary post-condition. The seeds above ran
    // immediately before, so a non-empty result means a write failed or a name
    // is otherwise non-insertable.
    let missing_effects: Vec<String> = sqlx::query_scalar(
        "SELECT v.name FROM UNNEST($1::text[]) AS v(name)
         WHERE NOT EXISTS (SELECT 1 FROM effect_catalog c WHERE c.name = v.name)",
    )
    .bind(seed_names(SEED_EFFECTS))
    .fetch_all(pool)
    .await?;
    let missing_type_tags: Vec<String> = sqlx::query_scalar(
        "SELECT v.name FROM UNNEST($1::text[]) AS v(name)
         WHERE NOT EXISTS (SELECT 1 FROM type_tag_catalog c WHERE c.name = v.name)",
    )
    .bind(seed_names(SEED_TYPE_TAGS))
    .fetch_all(pool)
    .await?;

    match (missing_effects.is_empty(), missing_type_tags.is_empty()) {
        (true, true) => info!(
            effects = SEED_EFFECTS.len(),
            type_tags = SEED_TYPE_TAGS.len(),
            "vocabulary catalogs reconciled (catalog ⊇ vocabulary verified)"
        ),
        _ => tracing::error!(
            ?missing_effects,
            ?missing_type_tags,
            "vocabulary catalog reconcile left a residual gap — symbol_effects / \
             type-tag inserts for the missing names will be rejected; investigate"
        ),
    }
    Ok(())
}

/// Owned `Vec` of a seed slice's `name` fields (preallocated) for binding as a
/// `text[]` parameter.
fn seed_names(seed: &'static [TagDef]) -> Vec<String> {
    let mut names = Vec::with_capacity(seed.len());
    names.extend(seed.iter().map(|t| t.name.to_string()));
    names
}

/// Idempotent upsert of a vocabulary catalog (`effect_catalog` /
/// `type_tag_catalog`) from its Rust seed slice. Shared by the v2 first-seed
/// (`v2_shadow_asr::seed_catalog_tables`) and the every-boot
/// [`reconcile_vocabulary_catalogs`] so both use one ON CONFLICT policy.
async fn seed_catalog(
    pool: &PgPool,
    table: &'static str,
    seed: &'static [TagDef],
) -> Result<(), sqlx::Error> {
    // Per-row upsert with ON CONFLICT DO UPDATE — keeps descriptions in sync as
    // the vocabulary evolves, and yields clearer error messages than a single
    // N-row VALUES list.
    let sql = format!(
        "INSERT INTO {table} (name, description, language_origin)
         VALUES ($1, $2, $3)
         ON CONFLICT (name) DO UPDATE SET
            description = EXCLUDED.description,
            language_origin = EXCLUDED.language_origin"
    );
    for entry in seed {
        sqlx::query(&sql)
            .bind(entry.name)
            .bind(entry.description)
            .bind(entry.origin.as_db_str())
            .execute(pool)
            .await?;
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
pub(crate) const MEMORY_UNIFIED_NODES_SQL: &str = "CREATE MATERIALIZED VIEW memory_unified_nodes AS
    SELECT 'memory_entity:' || id::TEXT AS node_id,
           'memory_entity'::TEXT AS node_type,
           name AS label,
           embedding,
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
      WHERE embedding_v2 IS NOT NULL
    UNION ALL
    SELECT 'file:' || id::TEXT, 'file',
           relative_path, NULL::VECTOR(1024), 0.4
      FROM indexed_files
    UNION ALL
    SELECT 'project:' || id::TEXT, 'project',
           name, NULL::VECTOR(1024), 0.8
      FROM projects
    UNION ALL
    SELECT 'symbol:' || s.id::TEXT, 'symbol',
           s.name, NULL::VECTOR(1024), 0.45
      FROM file_symbols s
      WHERE s.visibility = 'public'
         OR EXISTS (SELECT 1 FROM memory_code_anchor mca WHERE mca.symbol_id = s.id)
         OR EXISTS (SELECT 1 FROM work_item_code_anchor wca WHERE wca.symbol_id = s.id)
    UNION ALL
    SELECT 'work_item:' || id::TEXT, 'work_item',
           title, embedding, LEAST(1.0, 0.4 + priority / 200.0)
      FROM work_items
      WHERE status NOT IN ('cancelled','deferred')
    UNION ALL
    SELECT 'experiment:' || id::TEXT, 'experiment',
           title, embedding, 0.6
      FROM experiments WHERE valid_to IS NULL
    UNION ALL
    SELECT 'agent:' || aid, 'agent',
           aid, NULL::VECTOR(1024), 0.5
      FROM (
        SELECT agent_id AS aid FROM agent_presence
        UNION SELECT agent_id FROM work_item_claims
        UNION SELECT to_agent_id FROM work_item_claims WHERE to_agent_id IS NOT NULL
        UNION SELECT claimed_by FROM work_items WHERE claimed_by IS NOT NULL
      ) agents
      WHERE aid IS NOT NULL AND aid <> ''
    UNION ALL
    SELECT 'protocol:' || id::TEXT, 'protocol',
           name, NULL::VECTOR(1024), 0.6
      FROM csm_protocols
    UNION ALL
    SELECT 'protocol_role:' || id::TEXT, 'protocol_role',
           role, NULL::VECTOR(1024), 0.4
      FROM csm_projections
    UNION ALL
    -- effect catalog as graph nodes (shadow-ASR): lets PPR/RAPTOR cluster code
    -- by effect and lets traversal filter on effect membership. No embedding —
    -- effects are categorical hubs reached via `has_effect` edges, not seeded.
    SELECT 'effect:' || name, 'effect',
           name, NULL::VECTOR(1024), 0.3
      FROM effect_catalog
    UNION ALL
    -- type-tag catalog as graph nodes (shadow-ASR): the structural type vocabulary,
    -- reached via `has_type` edges from symbols' parameter / return tags.
    SELECT 'type_tag:' || name, 'type_tag',
           name, NULL::VECTOR(1024), 0.3
      FROM type_tag_catalog
    UNION ALL
    -- lock_resource nodes (ADR-011, concurrency): distinct lock resource_keys
    -- from the sync_ops skeleton. Categorical hubs reached via `acquires` /
    -- `lock_order` edges; no embedding.
    SELECT DISTINCT 'lock_resource:' || resource_key, 'lock_resource',
           resource_key, NULL::VECTOR(1024), 0.4
      FROM sync_ops
      WHERE resource_kind IN ('mutex', 'rwlock', 'condvar', 'semaphore', 'once')
        AND resource_key IS NOT NULL
    UNION ALL
    -- channel nodes (ADR-011): distinct message-channel resource_keys, reached
    -- via `sends_on` edges.
    SELECT DISTINCT 'channel:' || resource_key, 'channel',
           resource_key, NULL::VECTOR(1024), 0.3
      FROM sync_ops
      WHERE resource_kind = 'channel' AND resource_key IS NOT NULL
    UNION ALL
    -- v31 — A2A task HUB (non-embedded, like `commit`): surfaces skill/status as a
    -- label for graph traversal; reached via `in_task` / `evidenced_by` edges.
    SELECT 'a2a_task:' || id::TEXT, 'a2a_task',
           COALESCE(skill_id, status, 'task'), NULL::VECTOR(1024), 0.5
      FROM a2a_tasks
    UNION ALL
    -- v31 — A2A message (task transcript). Label = first 200 chars of the
    -- concatenated text parts; the embedding is cron-backfilled from the SAME text
    -- (jsonb text-part extraction), so label and vector never skew. COALESCE to
    -- `role` keeps a File/Data-only message labeled.
    SELECT 'a2a_message:' || m.id::TEXT, 'a2a_message',
           LEFT(COALESCE((
             SELECT string_agg(p->>'text', ' ' ORDER BY ord)
             FROM jsonb_array_elements(m.parts) WITH ORDINALITY AS e(p, ord)
             WHERE p->>'type' = 'text'
           ), m.role), 200),
           m.embedding, 0.45
      FROM a2a_messages m
      WHERE m.embedding IS NOT NULL
    UNION ALL
    -- v31 — agent social-mailbox message (v27). Label = subject — body prefix.
    SELECT 'agent_message:' || id::TEXT, 'agent_message',
           LEFT(COALESCE(subject || ' — ', '') || body, 200),
           embedding, 0.45
      FROM agent_messages
      WHERE embedding IS NOT NULL
    UNION ALL
    -- v31 — session prompt (already embedded in embedding_v2). Down-weighted (0.4)
    -- so prompts enrich recall without dominating default unified search.
    SELECT 'prompt:' || id::TEXT, 'prompt',
           LEFT(prompt_text, 200), embedding_v2, 0.4
      FROM session_prompts
      WHERE embedding_v2 IS NOT NULL
    UNION ALL
    -- v31 — JSON data table (v19), table-grain embedding (name+description, already
    -- populated by migrate_data_tables_batch; same BGE-M3 1024-d space). Rows reach
    -- via the table node + data_table_search.
    SELECT 'data_table:' || id::TEXT, 'data_table',
           COALESCE(name, 'table'), embedding, 0.5
      FROM data_tables
      WHERE embedding IS NOT NULL
    UNION ALL
    -- v31 — worktree-coordination negotiation (v29). Embeds reason/error_excerpt so
    -- a blocked negotiation is semantically findable; links work_item/agent/
    -- message/project via the edge arms below.
    SELECT 'coordination_request:' || id::TEXT, 'coordination_request',
           LEFT(COALESCE(reason, error_excerpt, status), 200), embedding, 0.55
      FROM coordination_requests
      WHERE embedding IS NOT NULL";

/// Edges-view definition. Same single-source-of-truth posture as
/// `MEMORY_UNIFIED_NODES_SQL` — F9's hash-gate covers both.
pub(crate) const MEMORY_UNIFIED_EDGES_SQL: &str = "CREATE MATERIALIZED VIEW memory_unified_edges AS
    -- Outer GROUP BY collapses parallel edges of the SAME type between the SAME
    -- pair into one row, so (from_id, to_id, edge_type) is UNIQUE — the unique
    -- index that REFRESH ... CONCURRENTLY requires. Several inner arms emit
    -- duplicate triples (code_graph_edges 'call' edges differing only by
    -- target_raw; work_item_claims handoff/claim rows differing only by
    -- created_at; the *_code_anchor arms; multiple active memory_relations), so
    -- a per-arm DISTINCT is insufficient. from_type/to_type are functionally
    -- determined by the *_id prefix (every arm builds '<type>:<pk>' and emits the
    -- matching literal type; trajectory_similarities stores both consistently),
    -- so grouping by them never splits a (from_id,to_id,edge_type) key into two
    -- rows — the unique index is safe. weight = MAX (bounded representative;
    -- also avoids PPR double-counting parallel edges); validity = earliest
    -- valid_from and an open (NULL) valid_to if ANY contributing edge is open.
    SELECT from_id, from_type, to_id, to_type, edge_type,
           MAX(weight) AS weight,
           MIN(valid_from) AS valid_from,
           CASE WHEN bool_or(valid_to IS NULL) THEN NULL ELSE MAX(valid_to) END AS valid_to
      FROM (
    -- memory entity ↔ entity (typed relations). Columns 7/8 = temporal validity
    -- interval (Stage 5a): bitemporal cols where available, created/computed
    -- timestamps elsewhere, NULL for timeless structural edges.
    SELECT 'memory_entity:' || from_entity_id::TEXT AS from_id,
           'memory_entity'::TEXT AS from_type,
           'memory_entity:' || to_entity_id::TEXT AS to_id,
           'memory_entity'::TEXT AS to_type,
           relation_type AS edge_type,
           importance::DOUBLE PRECISION AS weight,
           valid_from AS valid_from, valid_to AS valid_to
      FROM memory_relations WHERE valid_to IS NULL
    UNION ALL
    -- memory entity → code anchor (file/chunk/topic/symbol/project; v6 columns).
    -- NOTE: file_id now maps to 'file:' (was mislabeled 'chunk:').
    SELECT 'memory_entity:' || entity_id::TEXT,
           'memory_entity',
           CASE
             WHEN file_id    IS NOT NULL THEN 'file:'    || file_id::TEXT
             WHEN chunk_id   IS NOT NULL THEN 'chunk:'   || chunk_id::TEXT
             WHEN topic_id   IS NOT NULL THEN 'topic:'   || topic_id::TEXT
             WHEN symbol_id  IS NOT NULL THEN 'symbol:'  || symbol_id::TEXT
             ELSE 'project:' || project_id::TEXT
           END,
           CASE
             WHEN file_id    IS NOT NULL THEN 'file'
             WHEN chunk_id   IS NOT NULL THEN 'chunk'
             WHEN topic_id   IS NOT NULL THEN 'topic'
             WHEN symbol_id  IS NOT NULL THEN 'symbol'
             ELSE 'project'
           END,
           anchor_type,
           1.0::DOUBLE PRECISION,
           created_at, NULL::TIMESTAMPTZ
      FROM memory_code_anchor
    UNION ALL
    -- chunk → topic membership
    SELECT 'chunk:' || chunk_id::TEXT, 'chunk',
           'topic:' || topic_id::TEXT, 'topic',
           'belongs_to', membership_score::DOUBLE PRECISION,
           NULL::TIMESTAMPTZ, NULL::TIMESTAMPTZ
      FROM chunk_topic_assignments WHERE membership_score >= 0.05
    UNION ALL
    -- chunk → file containment
    SELECT 'chunk:' || id::TEXT, 'chunk',
           'file:' || file_id::TEXT, 'file',
           'in_file', 1.0::DOUBLE PRECISION,
           NULL::TIMESTAMPTZ, NULL::TIMESTAMPTZ
      FROM file_chunks WHERE file_id IS NOT NULL
    UNION ALL
    -- file → project containment
    SELECT 'file:' || id::TEXT, 'file',
           'project:' || project_id::TEXT, 'project',
           'in_project', 1.0::DOUBLE PRECISION,
           indexed_at, NULL::TIMESTAMPTZ
      FROM indexed_files WHERE project_id IS NOT NULL
    UNION ALL
    -- symbol → file (defined_in); gated to the same set as the symbol node arm
    SELECT 'symbol:' || s.id::TEXT, 'symbol',
           'file:' || s.file_id::TEXT, 'file',
           'defined_in', 1.0::DOUBLE PRECISION,
           NULL::TIMESTAMPTZ, NULL::TIMESTAMPTZ
      FROM file_symbols s
      WHERE s.visibility = 'public'
         OR EXISTS (SELECT 1 FROM memory_code_anchor mca WHERE mca.symbol_id = s.id)
         OR EXISTS (SELECT 1 FROM work_item_code_anchor wca WHERE wca.symbol_id = s.id)
    UNION ALL
    -- symbol → parent symbol (containment), same gate
    SELECT 'symbol:' || s.id::TEXT, 'symbol',
           'symbol:' || s.parent_id::TEXT, 'symbol',
           'parent_of', 1.0::DOUBLE PRECISION,
           NULL::TIMESTAMPTZ, NULL::TIMESTAMPTZ
      FROM file_symbols s
      WHERE s.parent_id IS NOT NULL
        AND (s.visibility = 'public'
         OR EXISTS (SELECT 1 FROM memory_code_anchor mca WHERE mca.symbol_id = s.id)
         OR EXISTS (SELECT 1 FROM work_item_code_anchor wca WHERE wca.symbol_id = s.id))
    UNION ALL
    -- symbol → symbol resolved CALL edges (shadow-ASR), weighted by resolution
    -- confidence. Both endpoints are gated to the symbol-node set (public/anchored,
    -- identical to the node arm) so no edge dangles, and the matview stays bounded.
    -- The 0.5 confidence floor keeps exact_in_file / exact_via_import /
    -- bare_name_unique and drops the 0.3 ambiguous guesses and 0.0 unresolved.
    -- Deep reachability through private helpers remains the domain of
    -- effect_propagation / dead_code_reachability over the full symbol_references
    -- graph; this arm contributes the public-API call structure to the unified graph.
    SELECT 'symbol:' || sr.source_symbol_id::TEXT, 'symbol',
           'symbol:' || sr.target_symbol_id::TEXT, 'symbol',
           'calls', sr.resolution_confidence::DOUBLE PRECISION,
           NULL::TIMESTAMPTZ, NULL::TIMESTAMPTZ
      FROM symbol_references sr
      JOIN file_symbols src ON src.id = sr.source_symbol_id
      JOIN file_symbols tgt ON tgt.id = sr.target_symbol_id
      WHERE sr.target_symbol_id IS NOT NULL
        AND sr.resolution_confidence >= 0.5
        AND (src.visibility = 'public'
             OR EXISTS (SELECT 1 FROM memory_code_anchor m WHERE m.symbol_id = src.id)
             OR EXISTS (SELECT 1 FROM work_item_code_anchor w WHERE w.symbol_id = src.id))
        AND (tgt.visibility = 'public'
             OR EXISTS (SELECT 1 FROM memory_code_anchor m WHERE m.symbol_id = tgt.id)
             OR EXISTS (SELECT 1 FROM work_item_code_anchor w WHERE w.symbol_id = tgt.id))
    UNION ALL
    -- symbol → effect membership (shadow-ASR); symbol gated to the node set.
    SELECT 'symbol:' || se.symbol_id::TEXT, 'symbol',
           'effect:' || se.effect, 'effect',
           'has_effect', 1.0::DOUBLE PRECISION,
           NULL::TIMESTAMPTZ, NULL::TIMESTAMPTZ
      FROM symbol_effects se
      JOIN file_symbols s ON s.id = se.symbol_id
      WHERE s.visibility = 'public'
         OR EXISTS (SELECT 1 FROM memory_code_anchor m WHERE m.symbol_id = s.id)
         OR EXISTS (SELECT 1 FROM work_item_code_anchor w WHERE w.symbol_id = s.id)
    UNION ALL
    -- symbol → type_tag (return-type tags, shadow-ASR); symbol gated to the node set.
    SELECT 'symbol:' || s.id::TEXT, 'symbol',
           'type_tag:' || tag, 'type_tag',
           'has_type', 1.0::DOUBLE PRECISION,
           NULL::TIMESTAMPTZ, NULL::TIMESTAMPTZ
      FROM file_symbols s
      CROSS JOIN LATERAL unnest(s.return_type_tags) AS tag
      WHERE s.visibility = 'public'
         OR EXISTS (SELECT 1 FROM memory_code_anchor m WHERE m.symbol_id = s.id)
         OR EXISTS (SELECT 1 FROM work_item_code_anchor w WHERE w.symbol_id = s.id)
    UNION ALL
    -- symbol → type_tag (parameter tags, shadow-ASR); symbol gated to the node set.
    SELECT 'symbol:' || sp.symbol_id::TEXT, 'symbol',
           'type_tag:' || tag, 'type_tag',
           'has_type', 1.0::DOUBLE PRECISION,
           NULL::TIMESTAMPTZ, NULL::TIMESTAMPTZ
      FROM symbol_parameters sp
      JOIN file_symbols s ON s.id = sp.symbol_id
      CROSS JOIN LATERAL unnest(sp.type_tags) AS tag
      WHERE s.visibility = 'public'
         OR EXISTS (SELECT 1 FROM memory_code_anchor m WHERE m.symbol_id = s.id)
         OR EXISTS (SELECT 1 FROM work_item_code_anchor w WHERE w.symbol_id = s.id)
    UNION ALL
    -- symbol → lock_resource: `acquires` (ADR-011). Timeless (static analysis).
    SELECT 'symbol:' || so.symbol_id::TEXT, 'symbol',
           'lock_resource:' || so.resource_key, 'lock_resource',
           'acquires', so.resource_confidence::DOUBLE PRECISION,
           NULL::TIMESTAMPTZ, NULL::TIMESTAMPTZ
      FROM sync_ops so
      WHERE so.op_kind IN ('acquire', 'acquire_read', 'acquire_write')
        AND so.resource_key IS NOT NULL
    UNION ALL
    -- symbol → channel: `sends_on` (ADR-011).
    SELECT 'symbol:' || so.symbol_id::TEXT, 'symbol',
           'channel:' || so.resource_key, 'channel',
           'sends_on', so.resource_confidence::DOUBLE PRECISION,
           NULL::TIMESTAMPTZ, NULL::TIMESTAMPTZ
      FROM sync_ops so
      WHERE so.op_kind IN ('send', 'send_persistent')
        AND so.resource_kind = 'channel' AND so.resource_key IS NOT NULL
    UNION ALL
    -- lock_resource → lock_resource: the BITEMPORAL `lock_order` edge (ADR-011),
    -- materialized by the concurrency-scan cron. `valid_from`/`valid_to` give
    -- each edge's analysis-history validity → `as_of`-queryable; a cycle that
    -- reappears after a close is a visible regression.
    SELECT 'lock_resource:' || from_key, 'lock_resource',
           'lock_resource:' || to_key, 'lock_resource',
           'lock_order', min_confidence::DOUBLE PRECISION,
           valid_from, valid_to
      FROM lock_order_edges
    UNION ALL
    -- project → project: the BITEMPORAL cross-project dependency edge (Phase 4).
    -- `valid_from`/`valid_to` make the dependency graph `as_of`-queryable and let
    -- MSM track cross-project coupling evolution over time.
    SELECT 'project:' || dependent_project_id::TEXT, 'project',
           'project:' || dependency_project_id::TEXT, 'project',
           'project_depends_on', confidence::DOUBLE PRECISION,
           valid_from, valid_to
      FROM project_dependencies
    UNION ALL
    -- file ↔ file: import / co_change / call (passthrough edge_type)
    SELECT 'file:' || source_file_id::TEXT, 'file',
           'file:' || target_file_id::TEXT, 'file',
           edge_type, LEAST(1.0, GREATEST(0.0, COALESCE(weight, 1.0)))::DOUBLE PRECISION,
           computed_at, NULL::TIMESTAMPTZ
      FROM code_graph_edges WHERE target_file_id IS NOT NULL
    UNION ALL
    -- chunk ↔ chunk cross-project similarity (near-duplicate threshold)
    SELECT 'chunk:' || chunk_id_a::TEXT, 'chunk',
           'chunk:' || chunk_id_b::TEXT, 'chunk',
           'similar_to', chunk_similarity::DOUBLE PRECISION,
           computed_at, NULL::TIMESTAMPTZ
      FROM cross_project_similarities WHERE chunk_similarity >= 0.80
    UNION ALL
    -- commit_chunk → commit
    SELECT 'commit_chunk:' || id::TEXT, 'commit_chunk',
           'commit:' || commit_id::TEXT, 'commit',
           'in_commit', 1.0::DOUBLE PRECISION,
           NULL::TIMESTAMPTZ, NULL::TIMESTAMPTZ
      FROM git_commit_chunks WHERE commit_id IS NOT NULL
    UNION ALL
    -- commit → file (touches): join file_path → indexed_files.relative_path per project
    SELECT 'commit:' || gc.id::TEXT, 'commit',
           'file:' || f.id::TEXT, 'file',
           'touches', 1.0::DOUBLE PRECISION,
           gc.author_date, NULL::TIMESTAMPTZ
      FROM git_commit_files gcf
      JOIN git_commits gc ON gc.id = gcf.commit_id
      JOIN indexed_files f
        ON f.project_id = gc.project_id AND f.relative_path = gcf.file_path
    UNION ALL
    -- work_item decomposition tree (parent → child)
    SELECT 'work_item:' || parent_id::TEXT, 'work_item',
           'work_item:' || id::TEXT, 'work_item',
           'parent_of', 1.0::DOUBLE PRECISION,
           created_at, NULL::TIMESTAMPTZ
      FROM work_items WHERE parent_id IS NOT NULL
    UNION ALL
    -- work_item ↔ work_item relations (DAG, passthrough type)
    SELECT 'work_item:' || from_item_id::TEXT, 'work_item',
           'work_item:' || to_item_id::TEXT, 'work_item',
           relation_type, 0.8::DOUBLE PRECISION,
           created_at, NULL::TIMESTAMPTZ
      FROM item_relations
    UNION ALL
    -- work_item → code anchors (file/chunk/symbol)
    SELECT 'work_item:' || item_id::TEXT, 'work_item',
           CASE
             WHEN file_id   IS NOT NULL THEN 'file:'   || file_id::TEXT
             WHEN chunk_id  IS NOT NULL THEN 'chunk:'  || chunk_id::TEXT
             ELSE 'symbol:' || symbol_id::TEXT
           END,
           CASE
             WHEN file_id   IS NOT NULL THEN 'file'
             WHEN chunk_id  IS NOT NULL THEN 'chunk'
             ELSE 'symbol'
           END,
           anchor_type, 1.0::DOUBLE PRECISION,
           created_at, NULL::TIMESTAMPTZ
      FROM work_item_code_anchor
    UNION ALL
    -- work_item → experiment (the existing Phase-10 bridge)
    SELECT 'work_item:' || work_item_id::TEXT, 'work_item',
           'experiment:' || experiment_id::TEXT, 'experiment',
           'validated_by', 1.0::DOUBLE PRECISION,
           created_at, NULL::TIMESTAMPTZ
      FROM work_item_experiment
    UNION ALL
    -- work_item → observation (memory-graph link, v6)
    SELECT 'work_item:' || id::TEXT, 'work_item',
           'observation:' || observation_id::TEXT, 'observation',
           'evidenced_by', 1.0::DOUBLE PRECISION,
           created_at, NULL::TIMESTAMPTZ
      FROM work_items WHERE observation_id IS NOT NULL
    UNION ALL
    -- work_item → project
    SELECT 'work_item:' || id::TEXT, 'work_item',
           'project:' || project_id::TEXT, 'project',
           'in_project', 0.5::DOUBLE PRECISION,
           created_at, NULL::TIMESTAMPTZ
      FROM work_items WHERE project_id IS NOT NULL
    UNION ALL
    -- experiment → code anchors (file/chunk/topic)
    SELECT 'experiment:' || experiment_id::TEXT, 'experiment',
           CASE
             WHEN file_id  IS NOT NULL THEN 'file:'  || file_id::TEXT
             WHEN chunk_id IS NOT NULL THEN 'chunk:' || chunk_id::TEXT
             ELSE 'topic:' || topic_id::TEXT
           END,
           CASE
             WHEN file_id  IS NOT NULL THEN 'file'
             WHEN chunk_id IS NOT NULL THEN 'chunk'
             ELSE 'topic'
           END,
           anchor_type, 1.0::DOUBLE PRECISION,
           created_at, NULL::TIMESTAMPTZ
      FROM experiment_code_anchor
    UNION ALL
    -- experiment → observation
    SELECT 'experiment:' || id::TEXT, 'experiment',
           'observation:' || observation_id::TEXT, 'observation',
           'evidenced_by', 1.0::DOUBLE PRECISION,
           valid_from, valid_to
      FROM experiments WHERE observation_id IS NOT NULL AND valid_to IS NULL
    UNION ALL
    -- experiment → project
    SELECT 'experiment:' || id::TEXT, 'experiment',
           'project:' || project_id::TEXT, 'project',
           'in_project', 0.5::DOUBLE PRECISION,
           valid_from, valid_to
      FROM experiments WHERE project_id IS NOT NULL AND valid_to IS NULL
    UNION ALL
    -- experiment supersession (a different experiment supersedes another)
    SELECT 'experiment:' || superseded_by::TEXT, 'experiment',
           'experiment:' || id::TEXT, 'experiment',
           'supersedes', 0.9::DOUBLE PRECISION,
           valid_from, valid_to
      FROM experiments WHERE superseded_by IS NOT NULL
    UNION ALL
    -- experiment ↔ experiment relations (DAG, passthrough type)
    SELECT 'experiment:' || from_experiment_id::TEXT, 'experiment',
           'experiment:' || to_experiment_id::TEXT, 'experiment',
           relation_type, 0.8::DOUBLE PRECISION,
           created_at, NULL::TIMESTAMPTZ
      FROM experiment_relations
    UNION ALL
    -- collaboration: work_item → current claimant agent
    SELECT 'work_item:' || id::TEXT, 'work_item',
           'agent:' || claimed_by, 'agent',
           'claimed_by', 0.7::DOUBLE PRECISION,
           claimed_at, NULL::TIMESTAMPTZ
      FROM work_items WHERE claimed_by IS NOT NULL AND claimed_by <> ''
    UNION ALL
    -- collaboration: agent → work_item currently focused on (presence)
    SELECT 'agent:' || agent_id, 'agent',
           'work_item:' || current_work_item_id::TEXT, 'work_item',
           'working_on', 0.6::DOUBLE PRECISION,
           last_active_at, NULL::TIMESTAMPTZ
      FROM agent_presence WHERE current_work_item_id IS NOT NULL
    UNION ALL
    -- collaboration: work_item ↔ agent claim history (distinct item/agent/action)
    SELECT DISTINCT 'work_item:' || work_item_id::TEXT, 'work_item',
           'agent:' || agent_id, 'agent',
           action, 0.5::DOUBLE PRECISION,
           created_at, NULL::TIMESTAMPTZ
      FROM work_item_claims WHERE agent_id IS NOT NULL AND agent_id <> ''
    UNION ALL
    -- collaboration: agent → agent handoffs
    SELECT DISTINCT 'agent:' || agent_id, 'agent',
           'agent:' || to_agent_id, 'agent',
           'handoff', 0.5::DOUBLE PRECISION,
           created_at, NULL::TIMESTAMPTZ
      FROM work_item_claims WHERE to_agent_id IS NOT NULL AND to_agent_id <> ''
    UNION ALL
    -- Stage 5c: MSM trajectory similarity — how records evolved over time (numeric)
    SELECT from_node_id, from_type, to_node_id, to_type,
           'evolves_like', weight,
           computed_at, NULL::TIMESTAMPTZ
      FROM trajectory_similarities WHERE edge_kind = 'evolves_like'
    UNION ALL
    -- Stage 5e: WFST/edit-distance workflow similarity — categorical event-sequence affinity
    SELECT from_node_id, from_type, to_node_id, to_type,
           'workflow_like', weight,
           computed_at, NULL::TIMESTAMPTZ
      FROM trajectory_similarities WHERE edge_kind = 'workflow_like'
    UNION ALL
    -- ADR-009: protocol → its per-role projection (the MPST G ↾ role)
    SELECT 'protocol:' || protocol_id::TEXT, 'protocol',
           'protocol_role:' || id::TEXT, 'protocol_role',
           'projects_to', 1.0::DOUBLE PRECISION,
           NULL::TIMESTAMPTZ, NULL::TIMESTAMPTZ
      FROM csm_projections
    UNION ALL
    -- v31 — a2a_message → its task (containment). valid_from = message created_at.
    SELECT 'a2a_message:' || id::TEXT, 'a2a_message',
           'a2a_task:' || task_id::TEXT, 'a2a_task',
           'in_task', 1.0::DOUBLE PRECISION,
           created_at, NULL::TIMESTAMPTZ
      FROM a2a_messages
    UNION ALL
    -- v31 — a2a_task → observation (evidenced_by) via agent_outcomes; restores the
    -- outcome→task context lost when outcomes were only mirrored into observations.
    -- DISTINCT collapses outcomes that share a (task, observation) pair; coexists
    -- with the work_item/experiment evidenced_by arms (disjoint from_id prefix).
    SELECT DISTINCT 'a2a_task:' || parent_task_id::TEXT, 'a2a_task',
           'observation:' || observation_id::TEXT, 'observation',
           'evidenced_by', 1.0::DOUBLE PRECISION,
           created_at, NULL::TIMESTAMPTZ
      FROM agent_outcomes
      WHERE parent_task_id IS NOT NULL AND observation_id IS NOT NULL
    UNION ALL
    -- v31 — agent_message reply chain. BITEMPORAL: created_at → valid_from,
    -- expires_at → valid_to (expired replies drop from an as_of=now() query).
    SELECT 'agent_message:' || id::TEXT, 'agent_message',
           'agent_message:' || reply_to::TEXT, 'agent_message',
           'reply_to', 0.6::DOUBLE PRECISION,
           created_at, expires_at
      FROM agent_messages WHERE reply_to IS NOT NULL
    UNION ALL
    -- v31 — agent → agent_message (authored). from_agent shares the TEXT `agent`
    -- namespace; gated to the SAME agent set as the `agent` node arm (IN-subquery)
    -- so the edge never dangles. Bitemporal (created_at/expires_at).
    SELECT 'agent:' || am.from_agent, 'agent',
           'agent_message:' || am.id::TEXT, 'agent_message',
           'sent', 0.5::DOUBLE PRECISION,
           am.created_at, am.expires_at
      FROM agent_messages am
      WHERE am.from_agent <> ''
        AND am.from_agent IN (
          SELECT agent_id FROM agent_presence
          UNION SELECT agent_id FROM work_item_claims
          UNION SELECT to_agent_id FROM work_item_claims WHERE to_agent_id IS NOT NULL
          UNION SELECT claimed_by FROM work_items WHERE claimed_by IS NOT NULL
        )
    UNION ALL
    -- v31 — session_mandate → its source prompt (extracted_from). Dense: every
    -- mandate has source_prompt_id.
    SELECT 'session_mandate:' || id::TEXT, 'session_mandate',
           'prompt:' || source_prompt_id::TEXT, 'prompt',
           'extracted_from', 0.7::DOUBLE PRECISION,
           created_at, NULL::TIMESTAMPTZ
      FROM session_mandates WHERE source_prompt_id IS NOT NULL
    UNION ALL
    -- v31 — data_table → its project.
    SELECT 'data_table:' || id::TEXT, 'data_table',
           'project:' || project_id::TEXT, 'project',
           'in_project', 0.5::DOUBLE PRECISION,
           created_at, NULL::TIMESTAMPTZ
      FROM data_tables WHERE project_id IS NOT NULL
    UNION ALL
    -- v31 — coordination_request → the blocked work_item (close-the-loop).
    -- BITEMPORAL: created_at → valid_from, resolved_at → valid_to.
    SELECT 'coordination_request:' || id::TEXT, 'coordination_request',
           'work_item:' || blocked_work_item_id::TEXT, 'work_item',
           'concerns', 0.7::DOUBLE PRECISION,
           created_at, resolved_at
      FROM coordination_requests WHERE blocked_work_item_id IS NOT NULL
    UNION ALL
    -- v31 — coordination_request → the mailbox message that carries it.
    SELECT 'coordination_request:' || id::TEXT, 'coordination_request',
           'agent_message:' || message_id::TEXT, 'agent_message',
           'concerns', 0.6::DOUBLE PRECISION,
           created_at, resolved_at
      FROM coordination_requests WHERE message_id IS NOT NULL
    UNION ALL
    -- v31 — requester agent → coordination_request (gated to the agent namespace).
    SELECT 'agent:' || cr.requester_agent, 'agent',
           'coordination_request:' || cr.id::TEXT, 'coordination_request',
           'requested', 0.5::DOUBLE PRECISION,
           cr.created_at, cr.resolved_at
      FROM coordination_requests cr
      WHERE cr.requester_agent IS NOT NULL AND cr.requester_agent <> ''
        AND cr.requester_agent IN (
          SELECT agent_id FROM agent_presence
          UNION SELECT agent_id FROM work_item_claims
          UNION SELECT to_agent_id FROM work_item_claims WHERE to_agent_id IS NOT NULL
          UNION SELECT claimed_by FROM work_items WHERE claimed_by IS NOT NULL
        )
    UNION ALL
    -- v31 — coordination_request → the dependency project being edited.
    SELECT 'coordination_request:' || id::TEXT, 'coordination_request',
           'project:' || dependency_project_id::TEXT, 'project',
           'in_project', 0.4::DOUBLE PRECISION,
           created_at, resolved_at
      FROM coordination_requests
      ) e
     GROUP BY from_id, from_type, to_id, to_type, edge_type";

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
/// Stage 5c: persistent MSM trajectory-similarity edges (`evolves_like`) — *how
/// records evolved over time*, captured by Move-Split-Merge over per-record
/// numeric trajectories (work-item progress %, file churn, experiment metrics).
/// Populated by the `trajectory-similarity` cron and surfaced as edges by
/// `memory_unified_edges`. Idempotent.
async fn ensure_trajectory_similarities(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS trajectory_similarities (
            id           BIGSERIAL PRIMARY KEY,
            from_node_id TEXT NOT NULL,
            from_type    TEXT NOT NULL,
            to_node_id   TEXT NOT NULL,
            to_type      TEXT NOT NULL,
            weight       DOUBLE PRECISION NOT NULL,
            msm_distance DOUBLE PRECISION NOT NULL,
            edge_kind    TEXT NOT NULL DEFAULT 'evolves_like',
            computed_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            UNIQUE (from_node_id, to_node_id, edge_kind)
        )",
    )
    .execute(pool)
    .await?;
    for idx in [
        "CREATE INDEX IF NOT EXISTS idx_traj_sim_from ON trajectory_similarities(from_node_id)",
        "CREATE INDEX IF NOT EXISTS idx_traj_sim_to ON trajectory_similarities(to_node_id)",
    ] {
        sqlx::query(idx).execute(pool).await?;
    }
    Ok(())
}

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

    // Drop the existing graph views so we can rebuild against the latest
    // column shapes. `memory_unified_edges` was a plain VIEW before Stage 2 and
    // is a MATERIALIZED VIEW after; a DO-block drops whichever form exists — a
    // bare `DROP MATERIALIZED VIEW IF EXISTS` ERRORs on a plain view (and
    // vice-versa) because IF EXISTS only suppresses "missing", not "wrong kind".
    sqlx::query(
        "DO $$
         BEGIN
            IF EXISTS (SELECT 1 FROM pg_class WHERE relname = 'memory_unified_edges' AND relkind = 'v') THEN
                DROP VIEW memory_unified_edges;
            ELSIF EXISTS (SELECT 1 FROM pg_class WHERE relname = 'memory_unified_edges' AND relkind = 'm') THEN
                DROP MATERIALIZED VIEW memory_unified_edges;
            END IF;
         END $$;",
    )
    .execute(pool)
    .await?;
    sqlx::query("DROP MATERIALIZED VIEW IF EXISTS memory_unified_nodes")
        .execute(pool)
        .await?;

    sqlx::query(MEMORY_UNIFIED_NODES_SQL).execute(pool).await?;
    // Unique index on the synthetic node_id ('<type>:<pk>', globally unique
    // across arms). REQUIRED for REFRESH MATERIALIZED VIEW CONCURRENTLY
    // (see refresh_memory_unified_nodes), and a useful point-lookup besides.
    sqlx::query(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_memory_unified_nodes_uq
            ON memory_unified_nodes (node_id)",
    )
    .execute(pool)
    .await?;
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
    // Unique index on the edge key. REQUIRED for REFRESH MATERIALIZED VIEW
    // CONCURRENTLY (see refresh_memory_unified_edges); the outer GROUP BY in
    // MEMORY_UNIFIED_EDGES_SQL guarantees (from_id, to_id, edge_type) is unique.
    // If a future edit reintroduces a duplicate triple, THIS build fails loudly
    // at boot — before any concurrent refresh can hit the duplicate.
    sqlx::query(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_memory_unified_edges_uq
            ON memory_unified_edges (from_id, to_id, edge_type)",
    )
    .execute(pool)
    .await?;
    // Edge-traversal indexes: the recursive-CTE walks
    // (`memory_neighbors`/`memory_path_search`) filter on
    // `from_id = current OR to_id = current`, so both endpoints need an index.
    // The edges view is materialized now, so without these a BFS hop would
    // seq-scan the entire edge set.
    for idx in [
        "CREATE INDEX IF NOT EXISTS idx_memory_unified_edges_from ON memory_unified_edges (from_id)",
        "CREATE INDEX IF NOT EXISTS idx_memory_unified_edges_to ON memory_unified_edges (to_id)",
        // Stage 5a: as-of / recency interval pruning on edge validity.
        "CREATE INDEX IF NOT EXISTS idx_memory_unified_edges_valid ON memory_unified_edges (valid_from, valid_to)",
    ] {
        sqlx::query(idx).execute(pool).await?;
    }

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

    // 1024-only cutover (ADR-005): pgmcp supports ONLY BGE-M3 1024d embeddings.
    // The legacy 384d MiniLM `embedding` column (and its HNSW index) is dropped
    // entirely on every dual-column table; `embedding_v2 vector(1024)` is the
    // sole canonical vector column. This DROP is idempotent across all three
    // states: a fresh DB (the column is no longer in the CREATE TABLE defs →
    // `IF EXISTS` no-ops), a mid-migration DB (drops the 384 column + index),
    // and an already-dropped DB (no-ops). DESTRUCTIVE BY DESIGN — any
    // remaining 384d MiniLM vectors are discarded (re-index produces 1024d).
    // The table/index names are a fixed literal allowlist ⇒ injection-safe.
    for (table, legacy_index) in [
        ("file_chunks", "idx_chunks_embedding"),
        ("session_prompts", "idx_session_prompts_embedding"),
        ("git_commit_chunks", "idx_git_commit_chunks_embedding"),
        (
            "software_pattern_chunks",
            "idx_software_pattern_chunks_embedding",
        ),
    ] {
        sqlx::query(&format!("DROP INDEX IF EXISTS {legacy_index}"))
            .execute(pool)
            .await?;
        sqlx::query(&format!(
            "ALTER TABLE {table} DROP COLUMN IF EXISTS embedding"
        ))
        .execute(pool)
        .await?;
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

/// Pin the `active_embedding_signature` row to `bge-m3-v1`. Post-ADR-005,
/// pgmcp is BGE-M3/1024-only — there is no MiniLM path to cut over from — so
/// this is force-set on every boot (`DO UPDATE`), upgrading any legacy DB that
/// still carried `minilm-l6-v2`. The row is retained purely as a forward-looking
/// breadcrumb for a future model swap; nothing selects a column from it anymore.
async fn ensure_active_embedding_signature(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO pgmcp_metadata (key, value)
         VALUES ('active_embedding_signature', 'bge-m3-v1')
         ON CONFLICT (key) DO UPDATE SET value = 'bge-m3-v1'",
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

    // Also gate on the legacy column's presence: post-cutover (ADR-005) the
    // `embedding` column is gone, so `CREATE INDEX … (embedding
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
