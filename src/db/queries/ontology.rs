//! Query layer for the hierarchical ontology (v23 sidecars).
//!
//! Concepts are `memory_entities`; this module reads/writes the
//! `ontology_concept_meta` sidecar and the file→concept / file→invariant lookups
//! the tools (Phase 6) and the digest/orient surfacing (Phase 7) need.
//!
//! **Trust boundary.** [`set_concept_status`] is the single status chokepoint:
//! an [`Actor::Agent`] may NOT move a concept to a curator-only status
//! (`accepted`/`canonical`) — mirroring the work-item tracker's no-Agent-arm
//! rule. Agents author `candidate`s and propose; a `user`/`gatekeeper` curates.

// Query helpers are wired across Phases 1/6/7; `upsert_concept_meta` has a caller
// now (the concept cron), the rest land with the tools. Allowed until then.
#![allow(dead_code)]

use serde::Serialize;
use sqlx::PgPool;

use crate::ontology::edge::EvidenceKind;
use crate::ontology::facet::{ConceptStatus, Facet};
use crate::tracker::transition::Actor;

/// Full `ontology_concept_meta` row.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct OntologyConceptMetaRow {
    pub entity_id: i64,
    pub facet: String,
    pub status: String,
    pub confidence: f32,
    pub constraint_text: Option<String>,
    pub rationale: Option<String>,
    pub sequence_spec: Option<String>,
    pub build_method: String,
    pub project_id: Option<i32>,
}

/// A concept + its facet/status, joined with its entity name. Returned by the
/// listing + file-lookup queries.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ConceptBriefRow {
    pub entity_id: i64,
    pub name: String,
    pub facet: String,
    pub status: String,
    pub confidence: f32,
}

/// An invariant concept anchored to a file/symbol — the Phase-7 surfacing shape.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct InvariantRow {
    pub entity_id: i64,
    pub name: String,
    pub constraint_text: Option<String>,
    pub rationale: Option<String>,
    pub status: String,
    pub confidence: f32,
}

/// Why a status change was refused.
#[derive(Debug, thiserror::Error)]
pub enum SetStatusError {
    /// An agent attempted to set a curator-only status (the trust boundary).
    #[error("an agent cannot set a curator-only status (accepted/canonical)")]
    AgentCannotCurate,
    /// No `ontology_concept_meta` row for that entity.
    #[error("no ontology concept metadata for entity {0}")]
    NotFound(i64),
    /// Underlying DB error.
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Upsert a concept's facet metadata. Idempotent and **curation-safe**: on
/// conflict it refreshes `facet`/`build_method` only while the concept is still a
/// `candidate`, so a curator-set `accepted`/`canonical` row (and any hand-edited
/// constraint/rationale) is never clobbered by a re-run of the auto-classifier.
pub async fn upsert_concept_meta(
    pool: &PgPool,
    entity_id: i64,
    facet: Facet,
    build_method: &str,
    project_id: Option<i32>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO ontology_concept_meta (entity_id, facet, build_method, project_id)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (entity_id) DO UPDATE
            SET facet = EXCLUDED.facet,
                build_method = EXCLUDED.build_method,
                project_id = COALESCE(ontology_concept_meta.project_id, EXCLUDED.project_id),
                updated_at = now()
          WHERE ontology_concept_meta.status = 'candidate'",
    )
    .bind(entity_id)
    .bind(facet.as_str())
    .bind(build_method)
    .bind(project_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Fetch a concept's metadata, if present.
pub async fn get_concept_meta(
    pool: &PgPool,
    entity_id: i64,
) -> Result<Option<OntologyConceptMetaRow>, sqlx::Error> {
    sqlx::query_as::<_, OntologyConceptMetaRow>(
        "SELECT entity_id, facet, status, confidence, constraint_text, rationale,
                sequence_spec, build_method, project_id
         FROM ontology_concept_meta WHERE entity_id = $1",
    )
    .bind(entity_id)
    .fetch_optional(pool)
    .await
}

/// The status chokepoint. An [`Actor::Agent`] is refused any curator-only status
/// ([`ConceptStatus::is_curator_only`]); `user`/`gatekeeper`/`system` may set any
/// status. Returns [`SetStatusError::NotFound`] if the concept has no meta row.
pub async fn set_concept_status(
    pool: &PgPool,
    entity_id: i64,
    status: ConceptStatus,
    actor: Actor,
) -> Result<(), SetStatusError> {
    if status.is_curator_only() && actor == Actor::Agent {
        return Err(SetStatusError::AgentCannotCurate);
    }
    let res = sqlx::query(
        "UPDATE ontology_concept_meta SET status = $2, updated_at = now() WHERE entity_id = $1",
    )
    .bind(entity_id)
    .bind(status.as_str())
    .execute(pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(SetStatusError::NotFound(entity_id));
    }
    Ok(())
}

/// List active concepts of one facet (optionally scoped to a project).
pub async fn list_concepts_by_facet(
    pool: &PgPool,
    facet: Facet,
    project_id: Option<i32>,
    limit: i64,
) -> Result<Vec<ConceptBriefRow>, sqlx::Error> {
    sqlx::query_as::<_, ConceptBriefRow>(
        "SELECT e.id AS entity_id, e.name, m.facet, m.status, m.confidence
         FROM ontology_concept_meta m
         JOIN memory_entities e ON e.id = m.entity_id AND e.valid_to IS NULL
         WHERE m.facet = $1 AND ($2::int IS NULL OR m.project_id = $2)
         ORDER BY (m.status = 'canonical') DESC, m.confidence DESC, e.id
         LIMIT $3",
    )
    .bind(facet.as_str())
    .bind(project_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// All concepts anchored to a file (directly, or via a symbol in that file).
pub async fn concepts_for_file(
    pool: &PgPool,
    file_id: i64,
) -> Result<Vec<ConceptBriefRow>, sqlx::Error> {
    sqlx::query_as::<_, ConceptBriefRow>(
        "SELECT DISTINCT e.id AS entity_id, e.name, m.facet, m.status, m.confidence
         FROM ontology_concept_meta m
         JOIN memory_entities e     ON e.id = m.entity_id AND e.valid_to IS NULL
         JOIN memory_code_anchor a  ON a.entity_id = m.entity_id
         LEFT JOIN file_symbols s   ON s.id = a.symbol_id
         WHERE a.file_id = $1 OR s.file_id = $1
         ORDER BY m.confidence DESC",
    )
    .bind(file_id)
    .fetch_all(pool)
    .await
}

/// Invariant concepts governing a file — the Phase-7 "constraint surfacing"
/// query. Canonical invariants first, then by confidence.
pub async fn invariants_for_file(
    pool: &PgPool,
    file_id: i64,
) -> Result<Vec<InvariantRow>, sqlx::Error> {
    sqlx::query_as::<_, InvariantRow>(
        "SELECT DISTINCT e.id AS entity_id, e.name, m.constraint_text, m.rationale,
                m.status, m.confidence
         FROM ontology_concept_meta m
         JOIN memory_entities e     ON e.id = m.entity_id AND e.valid_to IS NULL
         JOIN memory_code_anchor a  ON a.entity_id = m.entity_id
         LEFT JOIN file_symbols s   ON s.id = a.symbol_id
         WHERE m.facet = 'invariant' AND (a.file_id = $1 OR s.file_id = $1)
         ORDER BY (m.status = 'canonical') DESC, m.confidence DESC",
    )
    .bind(file_id)
    .fetch_all(pool)
    .await
}

/// Upsert an **invariant** concept's metadata (facet pinned to `invariant`, with
/// constraint + rationale). Curation-safe: on conflict it refreshes the
/// constraint/rationale only while the concept is still a `candidate`, so a
/// human-curated invariant is never overwritten by a re-mine.
pub async fn upsert_invariant_meta(
    pool: &PgPool,
    entity_id: i64,
    constraint_text: &str,
    rationale: &str,
    build_method: &str,
    project_id: Option<i32>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO ontology_concept_meta
            (entity_id, facet, constraint_text, rationale, build_method, project_id)
         VALUES ($1, 'invariant', $2, $3, $4, $5)
         ON CONFLICT (entity_id) DO UPDATE
            SET facet = 'invariant',
                constraint_text = EXCLUDED.constraint_text,
                rationale = EXCLUDED.rationale,
                build_method = EXCLUDED.build_method,
                project_id = COALESCE(ontology_concept_meta.project_id, EXCLUDED.project_id),
                updated_at = now()
          WHERE ontology_concept_meta.status = 'candidate'",
    )
    .bind(entity_id)
    .bind(constraint_text)
    .bind(rationale)
    .bind(build_method)
    .bind(project_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Insert an evidence pointer for a concept, idempotent on `provenance_key`
/// (so re-mines add nothing new). Returns `true` if a new row was inserted.
#[allow(clippy::too_many_arguments)]
pub async fn insert_concept_evidence(
    pool: &PgPool,
    entity_id: i64,
    kind: EvidenceKind,
    commit_id: Option<i64>,
    file_id: Option<i64>,
    mandate_ref: Option<&str>,
    detail: Option<&str>,
    provenance_key: &str,
) -> Result<bool, sqlx::Error> {
    let inserted: Option<i64> = sqlx::query_scalar(
        "INSERT INTO ontology_concept_evidence
            (entity_id, evidence_kind, commit_id, file_id, mandate_ref, detail, provenance_key)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (provenance_key) DO NOTHING
         RETURNING id",
    )
    .bind(entity_id)
    .bind(kind.as_str())
    .bind(commit_id)
    .bind(file_id)
    .bind(mandate_ref)
    .bind(detail)
    .bind(provenance_key)
    .fetch_optional(pool)
    .await?;
    Ok(inserted.is_some())
}

/// Count evidence rows for a concept (test/introspection helper).
pub async fn count_concept_evidence(pool: &PgPool, entity_id: i64) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM ontology_concept_evidence WHERE entity_id = $1")
        .bind(entity_id)
        .fetch_one(pool)
        .await
}
