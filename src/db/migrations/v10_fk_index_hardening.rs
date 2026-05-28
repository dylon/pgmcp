//! Migration step 10: `fk_index_hardening_v1`.
//!
//! ## Why
//!
//! PostgreSQL does **not** auto-create an index on the *referencing* (child)
//! column of a foreign key — only on the referenced (parent) side via the PK.
//! Every unindexed FK child column turns a cascading delete (or a `SET NULL`
//! fix-up, or a join through the FK) into a sequential scan of the child table.
//! pgmcp deletes from `indexed_files` / `file_chunks` / `file_symbols` /
//! `code_topics` constantly (hourly reindex cron + inotify), so each such delete
//! probes the child tables on the parent side of a cascade. This step adds the
//! missing indexes on the high- and medium-value FK child columns.
//!
//! Nullable/optional FK columns get a **partial** index (`WHERE col IS NOT
//! NULL`), matching the existing convention (`idx_memory_code_anchor_*`,
//! `idx_symbol_refs_source_symbol`, `idx_cge_source_symbol`). Naming follows
//! `idx_<table>_<col>`.
//!
//! ## Plus: a latent FK-action fix
//!
//! `memory_observations.source_session_id` (→ `sessions`) and `source_prompt_id`
//! (→ `session_prompts`) were declared with **no `ON DELETE` action** (= `NO
//! ACTION`). But `sessions → session_prompts` is `ON DELETE CASCADE`. The day a
//! session-retention/pruning job is added, `DELETE FROM sessions` will cascade
//! into `session_prompts` and be *blocked* by any observation still referencing
//! one of those prompts — failing the whole delete. We re-point both FKs to
//! `ON DELETE SET NULL`: an observation is valuable and survives; only the
//! provenance link is dropped (consistent with `experiments.observation_id`
//! etc.). No index is added on these two columns — they are queried by these
//! FK columns only during a (currently nonexistent) bulk session prune; add a
//! partial index alongside such a job if/when it lands.
//!
//! All `CREATE INDEX IF NOT EXISTS` statements and the two `confdeltype`-gated
//! `DO` blocks are idempotent, so the step is safe to re-run.

use sqlx::PgPool;

pub(super) const FK_INDEX_HARDENING_V1: i32 = 10;
pub(super) const FK_INDEX_HARDENING_V1_NAME: &str = "fk_index_hardening_v1";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    // ----------------------------------------------------------------
    // FK child-column indexes. None are HNSW, so plain execute is correct.
    // ----------------------------------------------------------------
    let indexes = [
        // ---- HIGH: large / high-churn referencing tables ----
        // symbol_references.target_file_id (SET NULL) — siblings target_symbol_id
        // & source_symbol_id already have partial indexes; this was the lone gap.
        "CREATE INDEX IF NOT EXISTS idx_symbol_refs_target_file \
            ON symbol_references(target_file_id) WHERE target_file_id IS NOT NULL",
        // cross_project_similarities.chunk_id_b (CASCADE) — idx_similarity_pair
        // leads with chunk_id_a (no b coverage); file_id_b/project_id_b are indexed.
        "CREATE INDEX IF NOT EXISTS idx_similarity_chunk_b \
            ON cross_project_similarities(chunk_id_b)",
        // file_symbols.parent_id (self-ref CASCADE) — parent/child walks + cascade.
        "CREATE INDEX IF NOT EXISTS idx_file_symbols_parent \
            ON file_symbols(parent_id) WHERE parent_id IS NOT NULL",
        // experiment_code_anchor.{file_id,chunk_id,topic_id} (CASCADE) — mirror the
        // memory_code_anchor / work_item_code_anchor partial indexes; the parent
        // side (file_chunks/indexed_files/code_topics) churns constantly.
        "CREATE INDEX IF NOT EXISTS idx_experiment_code_anchor_file \
            ON experiment_code_anchor(file_id) WHERE file_id IS NOT NULL",
        "CREATE INDEX IF NOT EXISTS idx_experiment_code_anchor_chunk \
            ON experiment_code_anchor(chunk_id) WHERE chunk_id IS NOT NULL",
        "CREATE INDEX IF NOT EXISTS idx_experiment_code_anchor_topic \
            ON experiment_code_anchor(topic_id) WHERE topic_id IS NOT NULL",
        // a2a_artifacts.task_id (CASCADE) — a2a_messages/events have (task_id,seq);
        // artifacts had no index at all.
        "CREATE INDEX IF NOT EXISTS idx_a2a_artifacts_task ON a2a_artifacts(task_id)",
        // ---- MEDIUM ----
        // code_topics.representative_chunk_id (SET NULL) — hourly chunk churn fires
        // the SET NULL fix-up, which otherwise seq-scans code_topics.
        "CREATE INDEX IF NOT EXISTS idx_code_topics_representative_chunk \
            ON code_topics(representative_chunk_id) WHERE representative_chunk_id IS NOT NULL",
        // durable_mandates.project_id (CASCADE) — not the leading column of
        // idx_durable_mandates_scope_project (scope, project_id), so uncovered.
        "CREATE INDEX IF NOT EXISTS idx_durable_mandates_project \
            ON durable_mandates(project_id) WHERE project_id IS NOT NULL",
        // Partial-index cascade gap: idx_memory_observations_active /
        // idx_memory_relations_from|to are partial (WHERE valid_to IS NULL). A HARD
        // delete of a memory_entity (memory_delete_entities / memory_forget) cascades
        // to ALL children incl. superseded (valid_to set) rows the partials don't
        // cover → seq scan. Non-partial FK indexes close it. (These overlap the
        // partials, adding ~3 small b-tree writes per insert on append-heavy tables;
        // justified because hard-delete cascades are real.)
        "CREATE INDEX IF NOT EXISTS idx_memory_observations_entity_all \
            ON memory_observations(entity_id)",
        "CREATE INDEX IF NOT EXISTS idx_memory_relations_from_all \
            ON memory_relations(from_entity_id)",
        "CREATE INDEX IF NOT EXISTS idx_memory_relations_to_all \
            ON memory_relations(to_entity_id)",
    ];
    for idx_sql in indexes {
        sqlx::query(idx_sql).execute(pool).await?;
    }

    // ----------------------------------------------------------------
    // Re-point memory_observations.{source_session_id, source_prompt_id} from
    // NO ACTION to ON DELETE SET NULL (see module doc for the latent
    // session-prune deadlock). Same idempotent, confdeltype-gated idiom as the
    // code_graph_edges re-tighten in migrations.rs: look up the FK name + current
    // action dynamically from pg_constraint and rewrite only when not already
    // SET NULL ('n'), so re-running against an already-fixed DB is a no-op.
    // confdeltype per Postgres docs: a=no action, r=restrict, c=cascade,
    // n=set null, d=set default.
    // ----------------------------------------------------------------
    sqlx::query(
        "DO $$
         DECLARE
            con_name    TEXT;
            con_deltype CHAR(1);
         BEGIN
            SELECT conname, confdeltype INTO con_name, con_deltype
              FROM pg_constraint c
              JOIN pg_class t ON t.oid = c.conrelid
              JOIN pg_attribute a
                ON a.attrelid = c.conrelid
               AND a.attnum   = ANY (c.conkey)
             WHERE t.relname = 'memory_observations'
               AND a.attname = 'source_session_id'
               AND c.contype = 'f'
             LIMIT 1;
            IF con_name IS NOT NULL AND con_deltype <> 'n' THEN
                EXECUTE format('ALTER TABLE memory_observations DROP CONSTRAINT %I', con_name);
                ALTER TABLE memory_observations
                    ADD CONSTRAINT memory_observations_source_session_id_fkey
                    FOREIGN KEY (source_session_id)
                    REFERENCES sessions(id)
                    ON DELETE SET NULL;
            END IF;
         END $$;",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "DO $$
         DECLARE
            con_name    TEXT;
            con_deltype CHAR(1);
         BEGIN
            SELECT conname, confdeltype INTO con_name, con_deltype
              FROM pg_constraint c
              JOIN pg_class t ON t.oid = c.conrelid
              JOIN pg_attribute a
                ON a.attrelid = c.conrelid
               AND a.attnum   = ANY (c.conkey)
             WHERE t.relname = 'memory_observations'
               AND a.attname = 'source_prompt_id'
               AND c.contype = 'f'
             LIMIT 1;
            IF con_name IS NOT NULL AND con_deltype <> 'n' THEN
                EXECUTE format('ALTER TABLE memory_observations DROP CONSTRAINT %I', con_name);
                ALTER TABLE memory_observations
                    ADD CONSTRAINT memory_observations_source_prompt_id_fkey
                    FOREIGN KEY (source_prompt_id)
                    REFERENCES session_prompts(id)
                    ON DELETE SET NULL;
            END IF;
         END $$;",
    )
    .execute(pool)
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        assert_eq!(FK_INDEX_HARDENING_V1, 10);
        assert_eq!(FK_INDEX_HARDENING_V1_NAME, "fk_index_hardening_v1");
    }
}
