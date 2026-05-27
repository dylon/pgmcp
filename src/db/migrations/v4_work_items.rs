//! Migration step 4: `work_items_v1` — the work-item / plan tracker schema.
//!
//! Realizes the tracker design at
//! `~/.claude/plans/plan-mcp-support-for-moonlit-dongarra.md`. A single
//! `work_items` spine (arbitrary-depth `parent_id` tree, closed `kind`/`status`
//! vocabularies) with orthogonal side tables for tags, progress, acceptance
//! criteria + evidence, plan definitions + rules, cross-tree relations, code
//! anchors, an append-only status-transition audit, and the user-only
//! deferral log (`scope_negotiations`).
//!
//! All tables ship together in one step so the FK graph is coherent at
//! migration completion — notably `work_item_status_history.evidence_id →
//! verification_evidence` and `.negotiation_id → scope_negotiations`, which
//! makes a `→verified` row without evidence (or a `→deferred` row without a
//! user negotiation) structurally impossible.
//!
//! Enum strategy per ADR-003: closed, *evolvable* vocabularies (kind, status,
//! origin, criterion_kind, evidence verdict/source, relation_type, rule_kind,
//! severity, def status, scope action) are `TEXT` columns + `CHECK (... IN
//! (...))` installed idempotently via `DROP CONSTRAINT IF EXISTS` + `ADD
//! CONSTRAINT` (the `session_mandates` idiom) so a future vocabulary edit is a
//! one-line constraint swap, never an `ALTER TYPE` on a populated enum. The
//! closed Rust mirrors live in `crate::tracker::{kind,status}` and a
//! `#[cfg(test)]` golden test asserts Rust ⇄ DB parity.
//!
//! Every statement is idempotent (`CREATE TABLE/INDEX IF NOT EXISTS`,
//! `ADD COLUMN IF NOT EXISTS`, `DROP CONSTRAINT IF EXISTS` + `ADD`), so the
//! step is safe to re-run; the runner gates it via `version_applied`.
//!
//! The `embedding vector(1024)` column on `work_items` is populated on write
//! and by the embedding-migration cron; its HNSW index is built separately in
//! `ensure_work_items_hnsw_index` (Phase 3) so the index-build tuning
//! (`maintenance_work_mem`, parallel workers) is shared with the other HNSW
//! builders rather than inlined here.

use sqlx::PgPool;

/// Step version number — must be unique across all migration steps
/// (1=initial, 2=shadow_asr, 3=cross_language_signatures).
pub(super) const WORK_ITEMS_V1: i32 = 4;
pub(super) const WORK_ITEMS_V1_NAME: &str = "work_items_v1";

/// Apply the `work_items_v1` step. Idempotent. Table creation order respects
/// the FK graph: `plan_definitions` → `work_items` → (tags/progress/criteria)
/// → `verification_evidence` + `scope_negotiations` → `work_item_status_history`
/// → relations/anchors.
pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    create_plan_definition_tables(pool).await?;
    create_work_items_table(pool).await?;
    install_work_items_checks(pool).await?;
    create_work_items_indexes(pool).await?;
    create_tag_tables(pool).await?;
    create_progress_table(pool).await?;
    create_acceptance_tables(pool).await?;
    create_scope_negotiations_table(pool).await?;
    create_status_history_table(pool).await?;
    create_relations_table(pool).await?;
    create_code_anchor_table(pool).await?;
    stamp_metadata(pool).await?;
    Ok(())
}

/// `plan_definitions` (reusable templates, DB source of truth) +
/// `definition_rules` (one typed row per dictated structural rule). Created
/// first because `work_items.definition_id` references `plan_definitions`.
async fn create_plan_definition_tables(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS plan_definitions (
            id          BIGSERIAL PRIMARY KEY,
            slug        TEXT NOT NULL,
            version     INTEGER NOT NULL DEFAULT 1,
            title       TEXT NOT NULL,
            description TEXT,
            extends_id  BIGINT REFERENCES plan_definitions(id) ON DELETE SET NULL,
            status      TEXT NOT NULL DEFAULT 'draft',
            body_toml   TEXT,
            created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            UNIQUE (slug, version)
        )",
    )
    .execute(pool)
    .await?;
    install_check(
        pool,
        "plan_definitions",
        "plan_definitions_status_check",
        "status IN ('draft','active','deprecated')",
    )
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_plan_def_active
            ON plan_definitions(slug) WHERE status = 'active'",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS definition_rules (
            id              BIGSERIAL PRIMARY KEY,
            definition_id   BIGINT NOT NULL REFERENCES plan_definitions(id) ON DELETE CASCADE,
            rule_kind       TEXT NOT NULL,
            applies_to_kind TEXT,
            child_kind      TEXT,
            min_count       INTEGER,
            max_count       INTEGER,
            field_name      TEXT,
            pattern         TEXT,
            severity        TEXT NOT NULL DEFAULT 'error',
            created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await?;
    install_check(
        pool,
        "definition_rules",
        "definition_rules_kind_check",
        "rule_kind IN ('required_kind','allowed_child_kind','required_child_kind',
            'min_children','max_children','required_field','required_acceptance_criterion',
            'quantifier_requires_corpus','naming_rule','id_rule','max_depth_advice')",
    )
    .await?;
    install_check(
        pool,
        "definition_rules",
        "definition_rules_severity_check",
        "severity IN ('error','warn','info')",
    )
    .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_def_rules_def ON definition_rules(definition_id)")
        .execute(pool)
        .await?;
    Ok(())
}

/// `work_items` — the node spine. Self-referential `parent_id` (arbitrary-depth
/// tree) and `root_id` (denormalized tree root for whole-plan reads). Inline
/// CHECKs cover the simple invariants; the evolvable vocabularies (kind/status/
/// origin) are installed separately in `install_work_items_checks`.
async fn create_work_items_table(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS work_items (
            id                  BIGSERIAL PRIMARY KEY,
            public_id           TEXT NOT NULL UNIQUE,
            parent_id           BIGINT REFERENCES work_items(id) ON DELETE CASCADE,
            project_id          INTEGER REFERENCES projects(id) ON DELETE SET NULL,
            definition_id       BIGINT REFERENCES plan_definitions(id) ON DELETE SET NULL,
            root_id             BIGINT REFERENCES work_items(id) ON DELETE CASCADE,
            kind                TEXT NOT NULL,
            status              TEXT NOT NULL DEFAULT 'pending',
            title               TEXT NOT NULL,
            body                TEXT,
            parametric          BOOLEAN NOT NULL DEFAULT FALSE,
            parametric_corpus   TEXT,
            parametric_expected INTEGER,
            priority            INTEGER NOT NULL DEFAULT 0,
            weight              REAL NOT NULL DEFAULT 1.0,
            computed_score      DOUBLE PRECISION,
            claimed_percent     SMALLINT NOT NULL DEFAULT 0,
            origin              TEXT NOT NULL DEFAULT 'user_explicit',
            created_by          TEXT,
            embedding           vector(1024),
            embedding_signature TEXT NOT NULL DEFAULT 'bge-m3-v1',
            created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            started_at          TIMESTAMPTZ,
            completed_at        TIMESTAMPTZ,
            verified_at         TIMESTAMPTZ,
            due_at              TIMESTAMPTZ,
            snooze_until        TIMESTAMPTZ,
            CHECK (parent_id IS NULL OR parent_id <> id),
            CHECK (claimed_percent BETWEEN 0 AND 100),
            CHECK (priority BETWEEN 0 AND 100),
            CHECK (weight > 0.0)
        )",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Evolvable-vocabulary CHECKs for `work_items` (kind / status / origin),
/// installed via DROP+ADD so a future vocabulary edit is a constraint swap.
/// `experiment` is in the kind set from v1 (the closed taxonomy is known now);
/// Phase 10 only adds the bridge + tools, not a kind.
pub(super) async fn install_work_items_checks(pool: &PgPool) -> Result<(), sqlx::Error> {
    // Built from the closed Rust enums so the enum is the single source of
    // truth (the v2_shadow_asr "seed from Rust vocabulary" idiom); a golden
    // test in each enum module pins the vocabulary.
    install_check(
        pool,
        "work_items",
        "work_items_kind_check",
        &format!("kind IN ({})", crate::tracker::kind::sql_in_list()),
    )
    .await?;
    install_check(
        pool,
        "work_items",
        "work_items_status_check",
        &format!("status IN ({})", crate::tracker::status::sql_in_list()),
    )
    .await?;
    install_check(
        pool,
        "work_items",
        "work_items_origin_check",
        "origin IN ('user_explicit','agent_write','ingest_plan','ingest_marker','migration')",
    )
    .await?;
    Ok(())
}

async fn create_work_items_indexes(pool: &PgPool) -> Result<(), sqlx::Error> {
    for idx in [
        "CREATE INDEX IF NOT EXISTS idx_work_items_parent ON work_items(parent_id)",
        "CREATE INDEX IF NOT EXISTS idx_work_items_root ON work_items(root_id)",
        "CREATE INDEX IF NOT EXISTS idx_work_items_project ON work_items(project_id) WHERE project_id IS NOT NULL",
        "CREATE INDEX IF NOT EXISTS idx_work_items_status ON work_items(status)",
        "CREATE INDEX IF NOT EXISTS idx_work_items_kind ON work_items(kind)",
        "CREATE INDEX IF NOT EXISTS idx_work_items_active ON work_items(status) \
            WHERE status IN ('pending','ready','in_progress','blocked')",
        "CREATE INDEX IF NOT EXISTS idx_work_items_priority \
            ON work_items(priority DESC, computed_score DESC NULLS LAST)",
        "CREATE INDEX IF NOT EXISTS idx_work_items_due ON work_items(due_at) \
            WHERE due_at IS NOT NULL AND status NOT IN ('verified','cancelled','deferred')",
    ] {
        sqlx::query(idx).execute(pool).await?;
    }
    Ok(())
}

/// `tags` catalog + `work_item_tags` join — custom, shared, many-to-many.
/// `merged_into` is the rename/merge tombstone; active queries filter it out.
async fn create_tag_tables(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS tags (
            id          BIGSERIAL PRIMARY KEY,
            name        TEXT NOT NULL,
            slug        TEXT NOT NULL UNIQUE,
            color       TEXT,
            description TEXT,
            created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            merged_into BIGINT REFERENCES tags(id) ON DELETE SET NULL
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_tags_active ON tags(slug) WHERE merged_into IS NULL",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS work_item_tags (
            item_id   BIGINT NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
            tag_id    BIGINT NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
            tagged_by TEXT,
            tagged_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            PRIMARY KEY (item_id, tag_id)
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_work_item_tags_tag ON work_item_tags(tag_id)")
        .execute(pool)
        .await?;
    Ok(())
}

/// `work_item_progress` — append-only progress log with provenance. The
/// activity-feed + collaboration write-back spine.
async fn create_progress_table(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS work_item_progress (
            id         BIGSERIAL PRIMARY KEY,
            item_id    BIGINT NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
            note       TEXT NOT NULL,
            percent    SMALLINT,
            provenance TEXT NOT NULL,
            actor_id   TEXT,
            session_id UUID REFERENCES sessions(id) ON DELETE SET NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            CHECK (percent IS NULL OR percent BETWEEN 0 AND 100)
        )",
    )
    .execute(pool)
    .await?;
    install_check(
        pool,
        "work_item_progress",
        "work_item_progress_provenance_check",
        "provenance IN ('user_explicit','agent_write')",
    )
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_wi_progress_item ON work_item_progress(item_id, created_at)",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// `acceptance_criteria` (machine-checkable spec) + `verification_evidence`
/// (append-only proof ledger — the trust anchor). The `gate` column reserves
/// the deferred α/β/γ Stop-hook plug with zero future schema churn.
async fn create_acceptance_tables(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS acceptance_criteria (
            id             BIGSERIAL PRIMARY KEY,
            item_id        BIGINT NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
            criterion_kind TEXT NOT NULL,
            description    TEXT NOT NULL,
            acceptance_uri TEXT,
            expect_exit    INTEGER DEFAULT 0,
            coverage_mode  TEXT NOT NULL DEFAULT 'single',
            gate           TEXT,
            required       BOOLEAN NOT NULL DEFAULT TRUE,
            created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            created_by     TEXT
        )",
    )
    .execute(pool)
    .await?;
    install_check(
        pool,
        "acceptance_criteria",
        "acceptance_criteria_kind_check",
        "criterion_kind IN ('test','build','lint','proof','model_check','smt','script',
            'auditor_verdict','manual_user_signoff','experiment_verdict')",
    )
    .await?;
    install_check(
        pool,
        "acceptance_criteria",
        "acceptance_criteria_coverage_check",
        "coverage_mode IN ('single','universal')",
    )
    .await?;
    install_check(
        pool,
        "acceptance_criteria",
        "acceptance_criteria_gate_check",
        "gate IS NULL OR gate IN ('alpha_antistub','beta_verify','gamma_audit','formal')",
    )
    .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_accept_crit_item ON acceptance_criteria(item_id)")
        .execute(pool)
        .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS verification_evidence (
            id              BIGSERIAL PRIMARY KEY,
            criterion_id    BIGINT NOT NULL REFERENCES acceptance_criteria(id) ON DELETE CASCADE,
            item_id         BIGINT NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
            verdict         TEXT NOT NULL,
            source          TEXT NOT NULL,
            exit_code       INTEGER,
            coverage_count  INTEGER,
            coverage_total  INTEGER,
            runner_identity TEXT,
            evidence_sha256 TEXT,
            commit_sha      TEXT,
            spec_sha256     TEXT,
            detail_json     JSONB NOT NULL DEFAULT '{}'::jsonb,
            created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await?;
    install_check(
        pool,
        "verification_evidence",
        "verification_evidence_verdict_check",
        "verdict IN ('pass','fail','unknown','error')",
    )
    .await?;
    install_check(
        pool,
        "verification_evidence",
        "verification_evidence_source_check",
        "source IN ('ci','stop_hook','subagent_audit','external_auditor','user_signoff',
            'experiment','manual')",
    )
    .await?;
    for idx in [
        "CREATE INDEX IF NOT EXISTS idx_evidence_criterion ON verification_evidence(criterion_id, created_at DESC)",
        "CREATE INDEX IF NOT EXISTS idx_evidence_item ON verification_evidence(item_id, created_at DESC)",
    ] {
        sqlx::query(idx).execute(pool).await?;
    }
    Ok(())
}

/// `scope_negotiations` — append-only, user-only deferral audit. The
/// `actor_kind = 'user'` CHECK is a structural statement of intent; the
/// tool layer enforces that an agent-authenticated caller cannot insert here.
async fn create_scope_negotiations_table(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS scope_negotiations (
            id         BIGSERIAL PRIMARY KEY,
            item_id    BIGINT NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
            action     TEXT NOT NULL,
            granted_by TEXT NOT NULL,
            actor_kind TEXT NOT NULL DEFAULT 'user',
            reason     TEXT NOT NULL,
            session_id UUID REFERENCES sessions(id) ON DELETE SET NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await?;
    install_check(
        pool,
        "scope_negotiations",
        "scope_negotiations_action_check",
        "action IN ('defer','reinstate','cancel','scope_cut')",
    )
    .await?;
    install_check(
        pool,
        "scope_negotiations",
        "scope_negotiations_actor_check",
        "actor_kind = 'user'",
    )
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_scope_neg_item ON scope_negotiations(item_id, created_at)",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// `work_item_status_history` — append-only transition audit. `evidence_id` is
/// required (enforced in the transition fn) for `→verified`; `negotiation_id`
/// for `→deferred`. Created after `verification_evidence` + `scope_negotiations`
/// so both FK targets exist.
async fn create_status_history_table(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS work_item_status_history (
            id             BIGSERIAL PRIMARY KEY,
            item_id        BIGINT NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
            from_status    TEXT,
            to_status      TEXT NOT NULL,
            actor_kind     TEXT NOT NULL,
            actor_id       TEXT,
            evidence_id    BIGINT REFERENCES verification_evidence(id) ON DELETE SET NULL,
            negotiation_id BIGINT REFERENCES scope_negotiations(id) ON DELETE SET NULL,
            reason         TEXT,
            created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await?;
    install_check(
        pool,
        "work_item_status_history",
        "work_item_status_history_actor_check",
        "actor_kind IN ('user','agent','gatekeeper','system')",
    )
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_wi_status_hist_item
            ON work_item_status_history(item_id, created_at)",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// `item_relations` — the blocks/depends_on DAG, orthogonal to the parent-FK
/// decomposition tree.
async fn create_relations_table(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS item_relations (
            id            BIGSERIAL PRIMARY KEY,
            from_item_id  BIGINT NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
            to_item_id    BIGINT NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
            relation_type TEXT NOT NULL,
            created_by    TEXT,
            created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            UNIQUE (from_item_id, to_item_id, relation_type),
            CHECK (from_item_id <> to_item_id)
        )",
    )
    .execute(pool)
    .await?;
    install_check(
        pool,
        "item_relations",
        "item_relations_type_check",
        "relation_type IN ('blocks','depends_on','relates_to','duplicates','supersedes','derived_from')",
    )
    .await?;
    for idx in [
        "CREATE INDEX IF NOT EXISTS idx_item_rel_from ON item_relations(from_item_id)",
        "CREATE INDEX IF NOT EXISTS idx_item_rel_to ON item_relations(to_item_id)",
    ] {
        sqlx::query(idx).execute(pool).await?;
    }
    Ok(())
}

/// `work_item_code_anchor` — tie an item/clause to code (file/chunk/symbol).
/// Same ≥1-FK CHECK shape as `memory_code_anchor`.
async fn create_code_anchor_table(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS work_item_code_anchor (
            id          BIGSERIAL PRIMARY KEY,
            item_id     BIGINT NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
            file_id     BIGINT REFERENCES indexed_files(id) ON DELETE CASCADE,
            chunk_id    BIGINT REFERENCES file_chunks(id) ON DELETE CASCADE,
            symbol_id   BIGINT REFERENCES file_symbols(id) ON DELETE CASCADE,
            anchor_type TEXT NOT NULL,
            created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            CHECK (file_id IS NOT NULL OR chunk_id IS NOT NULL OR symbol_id IS NOT NULL)
        )",
    )
    .execute(pool)
    .await?;
    for idx in [
        "CREATE INDEX IF NOT EXISTS idx_wi_anchor_item ON work_item_code_anchor(item_id)",
        "CREATE INDEX IF NOT EXISTS idx_wi_anchor_file ON work_item_code_anchor(file_id) WHERE file_id IS NOT NULL",
    ] {
        sqlx::query(idx).execute(pool).await?;
    }
    Ok(())
}

async fn stamp_metadata(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO pgmcp_metadata (key, value)
         VALUES ('work_items_version', '1')
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Install a named CHECK constraint idempotently (DROP IF EXISTS + ADD), the
/// `session_mandates` idiom (`src/db/migrations.rs`). Lets an evolvable
/// vocabulary be swapped on re-run without recreating the table.
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
        assert_eq!(WORK_ITEMS_V1, 4);
        assert_eq!(WORK_ITEMS_V1_NAME, "work_items_v1");
    }
}
