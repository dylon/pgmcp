//! Migration step 6: `unified_graph_v1` — schema foundation for the unified
//! knowledge-graph / ontology (the graph-RAG unification plan,
//! `~/.claude/plans/one-of-the-primary-mighty-galaxy.md`).
//!
//! Adds, on top of v4 (tracker) + v5 (collab) + the inline memory schema:
//! - `work_items.observation_id` → `memory_observations` (memory-graph reach,
//!   mirroring `experiments.observation_id`) so work items become KG-reachable;
//! - `experiment_relations` — the inter-experiment DAG (replicates/refutes/…),
//!   the structural twin of `item_relations`; vocabulary from the closed Rust
//!   enum `crate::experiment::relation::ExperimentRelation`;
//! - `memory_code_anchor.symbol_id` + `.project_id` (+ a relaxed, *named*
//!   ≥1-FK CHECK) so KG entities can anchor directly to symbols and projects;
//! - the `memory_source` enum gains `'auto_index'` — Stage-4 auto-population
//!   provenance (so auto-created concept entities never collide with the
//!   user/agent/reflection namespaces).
//!
//! NOT created here: the `work_item_experiment` bridge — it already exists
//! (`super::super::ensure_work_item_experiment_bridge`); Stage 2 wires it into
//! the unified graph view. Also NOT here: `ensure_work_items_hnsw_index`, which
//! already runs unconditionally in `run_migrations`.
//!
//! Every statement is idempotent (`ADD COLUMN IF NOT EXISTS`,
//! `CREATE TABLE/INDEX IF NOT EXISTS`, `ADD VALUE IF NOT EXISTS`, a guarded
//! `DO`-block for the anonymous-CHECK relaxation), so the step is safe to
//! re-run; the runner gates it via `version_applied`.

use sqlx::PgPool;

/// Step version number — unique across all migration steps
/// (1=initial, 2=shadow_asr, 3=cross_language_signatures, 4=work_items,
/// 5=work_items_collab).
pub(super) const UNIFIED_GRAPH_V1: i32 = 6;
pub(super) const UNIFIED_GRAPH_V1_NAME: &str = "unified_graph_v1";

/// Apply the `unified_graph_v1` step. Idempotent. Ordering is unconstrained —
/// every referenced table (`work_items`, `experiments`, `memory_observations`,
/// `memory_code_anchor`, `file_symbols`, `projects`) and the `memory_source`
/// type predate this step.
pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    add_work_item_observation_link(pool).await?;
    create_experiment_relations(pool).await?;
    extend_memory_code_anchor(pool).await?;
    add_auto_index_source(pool).await?;
    stamp_metadata(pool).await?;
    Ok(())
}

/// `work_items.observation_id → memory_observations` — the memory-graph reach,
/// mirroring `experiments.observation_id`. `ON DELETE SET NULL` so deleting an
/// observation never deletes the work item. Partial index for the active link.
async fn add_work_item_observation_link(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "ALTER TABLE work_items ADD COLUMN IF NOT EXISTS observation_id BIGINT \
         REFERENCES memory_observations(id) ON DELETE SET NULL",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_work_items_observation \
         ON work_items(observation_id) WHERE observation_id IS NOT NULL",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// `experiment_relations` — the blocks/depends-style DAG for experiments,
/// orthogonal to the `superseded_by` version chain. Closed `relation_type`
/// vocabulary from `crate::experiment::relation` (the `item_relations` idiom).
async fn create_experiment_relations(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS experiment_relations (
            id                 BIGSERIAL PRIMARY KEY,
            from_experiment_id BIGINT NOT NULL REFERENCES experiments(id) ON DELETE CASCADE,
            to_experiment_id   BIGINT NOT NULL REFERENCES experiments(id) ON DELETE CASCADE,
            relation_type      TEXT NOT NULL,
            created_by         TEXT,
            created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            UNIQUE (from_experiment_id, to_experiment_id, relation_type),
            CHECK (from_experiment_id <> to_experiment_id)
        )",
    )
    .execute(pool)
    .await?;
    install_check(
        pool,
        "experiment_relations",
        "experiment_relations_type_check",
        &format!(
            "relation_type IN ({})",
            crate::experiment::relation::sql_in_list()
        ),
    )
    .await?;
    for idx in [
        "CREATE INDEX IF NOT EXISTS idx_exp_rel_from ON experiment_relations(from_experiment_id)",
        "CREATE INDEX IF NOT EXISTS idx_exp_rel_to ON experiment_relations(to_experiment_id)",
    ] {
        sqlx::query(idx).execute(pool).await?;
    }
    Ok(())
}

/// Extend `memory_code_anchor` with `symbol_id`/`project_id` so KG entities can
/// anchor to symbols/projects, and relax the ≥1-FK CHECK to include them.
///
/// The original CHECK is an inline *anonymous* constraint (auto-named by PG), so
/// we cannot `DROP CONSTRAINT IF EXISTS <known_name>` reliably. We use the
/// proven `pg_constraint`-lookup `DO`-block idiom (cf. the `code_graph_edges`
/// FK fix): drop any non-target CHECK on the table, then add the *named*
/// relaxed constraint when absent. Idempotent on re-run.
async fn extend_memory_code_anchor(pool: &PgPool) -> Result<(), sqlx::Error> {
    for stmt in [
        "ALTER TABLE memory_code_anchor ADD COLUMN IF NOT EXISTS symbol_id BIGINT \
         REFERENCES file_symbols(id) ON DELETE CASCADE",
        "ALTER TABLE memory_code_anchor ADD COLUMN IF NOT EXISTS project_id INTEGER \
         REFERENCES projects(id) ON DELETE CASCADE",
    ] {
        sqlx::query(stmt).execute(pool).await?;
    }
    sqlx::query(
        "DO $$
         DECLARE con_name TEXT;
         BEGIN
            FOR con_name IN
                SELECT conname FROM pg_constraint
                 WHERE conrelid = 'memory_code_anchor'::regclass
                   AND contype  = 'c'
                   AND conname <> 'memory_code_anchor_target_check'
            LOOP
                EXECUTE format('ALTER TABLE memory_code_anchor DROP CONSTRAINT %I', con_name);
            END LOOP;
            IF NOT EXISTS (
                SELECT 1 FROM pg_constraint
                 WHERE conrelid = 'memory_code_anchor'::regclass
                   AND conname  = 'memory_code_anchor_target_check'
            ) THEN
                ALTER TABLE memory_code_anchor
                    ADD CONSTRAINT memory_code_anchor_target_check
                    CHECK (file_id IS NOT NULL OR chunk_id IS NOT NULL OR topic_id IS NOT NULL
                           OR symbol_id IS NOT NULL OR project_id IS NOT NULL);
            END IF;
         END $$;",
    )
    .execute(pool)
    .await?;
    for idx in [
        "CREATE INDEX IF NOT EXISTS idx_memory_code_anchor_symbol \
         ON memory_code_anchor(symbol_id) WHERE symbol_id IS NOT NULL",
        "CREATE INDEX IF NOT EXISTS idx_memory_code_anchor_project \
         ON memory_code_anchor(project_id) WHERE project_id IS NOT NULL",
    ] {
        sqlx::query(idx).execute(pool).await?;
    }
    Ok(())
}

/// Add `'auto_index'` to the `memory_source` enum — Stage-4 auto-population
/// provenance. `ADD VALUE IF NOT EXISTS` is idempotent (PG 12+) and runs in
/// autocommit (each `sqlx::query` is its own statement), so the new value is
/// committed before any later migration could use it.
async fn add_auto_index_source(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query("ALTER TYPE memory_source ADD VALUE IF NOT EXISTS 'auto_index'")
        .execute(pool)
        .await?;
    Ok(())
}

async fn stamp_metadata(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO pgmcp_metadata (key, value)
         VALUES ('unified_graph_version', '1')
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Install a named CHECK constraint idempotently (DROP IF EXISTS + ADD) — the
/// `session_mandates`/`v4_work_items` idiom. Lets an evolvable vocabulary be
/// swapped on re-run without recreating the table.
async fn install_check(
    pool: &PgPool,
    table: &str,
    constraint: &str,
    predicate: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(&format!(
        "ALTER TABLE {table} DROP CONSTRAINT IF EXISTS {constraint}"
    ))
    .execute(pool)
    .await?;
    sqlx::query(&format!(
        "ALTER TABLE {table} ADD CONSTRAINT {constraint} CHECK ({predicate})"
    ))
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        // Pinning the constant — changing it is a schema-breaking event.
        assert_eq!(UNIFIED_GRAPH_V1, 6);
        assert_eq!(UNIFIED_GRAPH_V1_NAME, "unified_graph_v1");
    }
}
