//! Migration step 23: hierarchical-ontology sidecars.
//!
//! Five tables that layer a multi-faceted concept hierarchy onto the existing
//! unified memory graph WITHOUT touching the node/edge matviews: concepts are
//! `memory_entities` (`entity_type='concept'`); the
//! `is_a`/`part_of`/`broader`/`narrower`/`member_of` edges ride
//! `memory_relations.relation_type` (already a documented passthrough in
//! [`crate::db::ontology::FREEFORM_EDGE_SOURCES`]). These sidecars carry only the
//! *new* facet / invariant / curation metadata.
//!
//! - `ontology_concept_meta` — 1:1 with a concept entity: facet + curation status
//!   + confidence + invariant constraint/rationale + optional WFST `sequence_spec`
//!   + build provenance + project scope. `facet`/`status` are closed vocabularies
//!   (ADR-003): `TEXT` + `CHECK` from
//!   [`crate::ontology::facet`]`::{facet,status}_sql_in_list()`.
//! - `ontology_concept_evidence` — Code-Digital-Twin `constrained-by`/`justified-by`
//!   provenance pointers; `evidence_kind` closed vocab from
//!   [`crate::ontology::edge::evidence_sql_in_list`]; `provenance_key` UNIQUE for
//!   idempotent mining (mirrors the v15 effect-drift / v22 concurrency ledgers).
//! - `ontology_concept_attr` — small structured key/value facts on a concept.
//! - `ontology_data_link` — links a concept to a v19 `data_tables` row (tabular
//!   non-code data: hardware inventory, tool registry).
//! - `ontology_rule` — the user-extensible egglog/Datalog ruleset (Phase 9).

use sqlx::PgPool;

use crate::ontology::edge::evidence_sql_in_list;
use crate::ontology::facet::{facet_sql_in_list, status_sql_in_list};

pub(super) const ONTOLOGY_V1: i32 = 23;
pub(super) const ONTOLOGY_V1_NAME: &str = "ontology_v1";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout = 0")
        .execute(&mut *tx)
        .await?;

    // 1. Concept-metadata sidecar (1:1 with the concept entity).
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS ontology_concept_meta (
            entity_id       BIGINT PRIMARY KEY REFERENCES memory_entities(id) ON DELETE CASCADE,
            facet           TEXT NOT NULL,
            status          TEXT NOT NULL DEFAULT 'candidate',
            confidence      REAL NOT NULL DEFAULT 0.5,
            constraint_text TEXT,
            rationale       TEXT,
            sequence_spec   TEXT,
            build_method    TEXT NOT NULL DEFAULT 'agent',
            project_id      INTEGER REFERENCES projects(id) ON DELETE CASCADE,
            created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
            updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
            CHECK (confidence >= 0.0 AND confidence <= 1.0)
        )",
    )
    .execute(&mut *tx)
    .await?;

    // Closed-vocab CHECKs, idempotent DROP+ADD so fresh and upgraded installs
    // converge on the current enum (the ADR-003 idiom).
    for (name, col, list) in [
        ("chk_ontology_meta_facet", "facet", facet_sql_in_list()),
        ("chk_ontology_meta_status", "status", status_sql_in_list()),
    ] {
        sqlx::query(&format!(
            "ALTER TABLE ontology_concept_meta DROP CONSTRAINT IF EXISTS {name}"
        ))
        .execute(&mut *tx)
        .await?;
        sqlx::query(&format!(
            "ALTER TABLE ontology_concept_meta ADD CONSTRAINT {name} CHECK ({col} IN ({list}))"
        ))
        .execute(&mut *tx)
        .await?;
    }

    // 2. Evidence pointers (Code-Digital-Twin constrained-by / justified-by).
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS ontology_concept_evidence (
            id             BIGSERIAL PRIMARY KEY,
            entity_id      BIGINT NOT NULL REFERENCES memory_entities(id) ON DELETE CASCADE,
            evidence_kind  TEXT NOT NULL,
            commit_id      BIGINT REFERENCES git_commits(id) ON DELETE CASCADE,
            file_id        BIGINT REFERENCES indexed_files(id) ON DELETE CASCADE,
            mandate_ref    TEXT,
            detail         TEXT,
            provenance_key TEXT NOT NULL UNIQUE,
            created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
        )",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "ALTER TABLE ontology_concept_evidence DROP CONSTRAINT IF EXISTS chk_ontology_evidence_kind",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query(&format!(
        "ALTER TABLE ontology_concept_evidence
         ADD CONSTRAINT chk_ontology_evidence_kind CHECK (evidence_kind IN ({}))",
        evidence_sql_in_list()
    ))
    .execute(&mut *tx)
    .await?;

    // 3. Small structured attributes (e.g. z3.version, z3.path).
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS ontology_concept_attr (
            entity_id  BIGINT NOT NULL REFERENCES memory_entities(id) ON DELETE CASCADE,
            key        TEXT NOT NULL,
            value      TEXT NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
            PRIMARY KEY (entity_id, key)
        )",
    )
    .execute(&mut *tx)
    .await?;

    // 4. Link a concept to a v19 data table (tabular non-code data).
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS ontology_data_link (
            entity_id     BIGINT NOT NULL REFERENCES memory_entities(id) ON DELETE CASCADE,
            data_table_id BIGINT NOT NULL REFERENCES data_tables(id) ON DELETE CASCADE,
            created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
            PRIMARY KEY (entity_id, data_table_id)
        )",
    )
    .execute(&mut *tx)
    .await?;

    // 5. User-extensible egglog/Datalog rules (materialized by the Phase-9 engine).
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS ontology_rule (
            id         BIGSERIAL PRIMARY KEY,
            name       TEXT NOT NULL UNIQUE,
            head       TEXT NOT NULL,
            body       TEXT NOT NULL,
            enabled    BOOLEAN NOT NULL DEFAULT TRUE,
            source     TEXT NOT NULL DEFAULT 'user_explicit',
            created_at TIMESTAMPTZ NOT NULL DEFAULT now()
        )",
    )
    .execute(&mut *tx)
    .await?;

    for stmt in [
        "CREATE INDEX IF NOT EXISTS idx_ontology_meta_facet
            ON ontology_concept_meta (facet)",
        "CREATE INDEX IF NOT EXISTS idx_ontology_meta_project
            ON ontology_concept_meta (project_id) WHERE project_id IS NOT NULL",
        // Hot path: surface invariants for a file (Phase 7 digest / orient).
        "CREATE INDEX IF NOT EXISTS idx_ontology_meta_invariant
            ON ontology_concept_meta (facet) WHERE facet = 'invariant'",
        "CREATE INDEX IF NOT EXISTS idx_ontology_evidence_entity
            ON ontology_concept_evidence (entity_id)",
        "CREATE INDEX IF NOT EXISTS idx_ontology_evidence_commit
            ON ontology_concept_evidence (commit_id) WHERE commit_id IS NOT NULL",
        "CREATE INDEX IF NOT EXISTS idx_ontology_evidence_file
            ON ontology_concept_evidence (file_id) WHERE file_id IS NOT NULL",
        "CREATE INDEX IF NOT EXISTS idx_ontology_data_link_table
            ON ontology_data_link (data_table_id)",
    ] {
        sqlx::query(stmt).execute(&mut *tx).await?;
    }

    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        assert_eq!(ONTOLOGY_V1, 23);
        assert_eq!(ONTOLOGY_V1_NAME, "ontology_v1");
    }

    #[test]
    fn checks_quote_the_vocabulary() {
        assert!(facet_sql_in_list().contains("'invariant'"));
        assert!(facet_sql_in_list().contains("'collection'"));
        assert!(status_sql_in_list().contains("'canonical'"));
        assert!(evidence_sql_in_list().contains("'data_table'"));
    }
}
