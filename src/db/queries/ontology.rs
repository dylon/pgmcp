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

use crate::ontology::edge::{EvidenceKind, OntologyRelation};
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

/// Insert a hierarchy edge (`is_a`/`part_of`/`broader`/`narrower`/`member_of`)
/// between two concept entities via the freeform `memory_relations.relation_type`
/// passthrough. Idempotent on the active (`valid_to IS NULL`) triple and
/// self-edge-safe (the table's CHECK forbids `from = to`). Returns `true` if a
/// new edge was inserted.
pub async fn insert_ontology_edge(
    pool: &PgPool,
    from_entity_id: i64,
    to_entity_id: i64,
    relation: OntologyRelation,
    weight: f64,
) -> Result<bool, sqlx::Error> {
    if from_entity_id == to_entity_id {
        return Ok(false);
    }
    let id: Option<i64> = sqlx::query_scalar(
        "INSERT INTO memory_relations
            (from_entity_id, to_entity_id, relation_type, importance, source)
         SELECT $1, $2, $3, $4, 'auto_index'::memory_source
         WHERE NOT EXISTS (
             SELECT 1 FROM memory_relations
             WHERE from_entity_id = $1 AND to_entity_id = $2
               AND relation_type = $3 AND valid_to IS NULL
         )
         RETURNING id",
    )
    .bind(from_entity_id)
    .bind(to_entity_id)
    .bind(relation.as_str())
    .bind(weight as f32)
    .fetch_optional(pool)
    .await?;
    Ok(id.is_some())
}

/// All active concept entity-ids of one facet (including those with no code
/// attributes — they become the most-general nodes in the `is_a` poset).
pub async fn list_concept_ids_by_facet(
    pool: &PgPool,
    facet: Facet,
) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT m.entity_id FROM ontology_concept_meta m
         JOIN memory_entities e ON e.id = m.entity_id AND e.valid_to IS NULL
         WHERE m.facet = $1
         ORDER BY m.entity_id",
    )
    .bind(facet.as_str())
    .fetch_all(pool)
    .await
}

/// `(entity_id, finding_id, title, file_id)` pairs where a concurrency finding
/// (v22) touches a file/symbol that an ontology concept is anchored to — the
/// Phase-11 producer-integration join (analyzer findings → concept evidence).
pub async fn concept_concurrency_findings(
    pool: &PgPool,
) -> Result<Vec<(i64, i64, String, Option<i64>)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT DISTINCT m.entity_id, cf.id, cf.title, cf.file_id
         FROM concurrency_findings cf
         JOIN memory_code_anchor a
           ON (cf.file_id   IS NOT NULL AND a.file_id   = cf.file_id)
           OR (cf.symbol_id IS NOT NULL AND a.symbol_id = cf.symbol_id)
         JOIN ontology_concept_meta m ON m.entity_id = a.entity_id
         ORDER BY m.entity_id, cf.id",
    )
    .fetch_all(pool)
    .await
}

/// Software-pattern catalog rows for migration: `(slug, name, category, kind)`.
pub async fn load_software_patterns(
    pool: &PgPool,
) -> Result<Vec<(String, String, String, String)>, sqlx::Error> {
    sqlx::query_as("SELECT slug, name, category, kind FROM software_patterns ORDER BY id")
        .fetch_all(pool)
        .await
}

/// Get-or-create a `source='migration'` concept entity (by active name) + its
/// facet metadata — the Phase-10 taxonomy-import path. Returns `(entity_id,
/// created)`. Curation-safe (the meta upsert only refreshes a `candidate`).
pub async fn migrate_concept(
    pool: &PgPool,
    name: &str,
    facet: Facet,
    build_method: &str,
) -> Result<(i64, bool), sqlx::Error> {
    let existing: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM memory_entities \
         WHERE name = $1 AND entity_type = 'concept' AND valid_to IS NULL LIMIT 1",
    )
    .bind(name)
    .fetch_optional(pool)
    .await?;
    let (entity_id, created) = match existing {
        Some(id) => (id, false),
        None => {
            let id: i64 = sqlx::query_scalar(
                "INSERT INTO memory_entities (name, entity_type, source) \
                 VALUES ($1, 'concept', 'migration'::memory_source) RETURNING id",
            )
            .bind(name)
            .fetch_one(pool)
            .await?;
            (id, true)
        }
    };
    upsert_concept_meta(pool, entity_id, facet, build_method, None).await?;
    Ok((entity_id, created))
}

/// Upsert a small structured attribute on a concept (`ontology_concept_attr`).
pub async fn set_concept_attr(
    pool: &PgPool,
    entity_id: i64,
    key: &str,
    value: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO ontology_concept_attr (entity_id, key, value) VALUES ($1, $2, $3)
         ON CONFLICT (entity_id, key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(entity_id)
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

/// Concept entity-ids that lie on an `is_a` cycle (reachable from themselves).
/// The recursive CTE uses `UNION` (set semantics) so it terminates even when a
/// cycle exists. An empty result means the `is_a` graph is a DAG.
pub async fn detect_is_a_cycles(pool: &PgPool) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "WITH RECURSIVE reach(start, node) AS (
             SELECT from_entity_id, to_entity_id FROM memory_relations
             WHERE relation_type = 'is_a' AND valid_to IS NULL
             UNION
             SELECT r.start, m.to_entity_id
             FROM reach r
             JOIN memory_relations m
               ON m.from_entity_id = r.node AND m.relation_type = 'is_a' AND m.valid_to IS NULL
         )
         SELECT DISTINCT start FROM reach WHERE start = node ORDER BY start",
    )
    .fetch_all(pool)
    .await
}

/// Invariant concepts with no code anchor — a constraint violation (an invariant
/// must govern ≥1 file/symbol to be actionable). Returns `(entity_id, name)`.
pub async fn unanchored_invariants(pool: &PgPool) -> Result<Vec<(i64, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT m.entity_id, e.name
         FROM ontology_concept_meta m
         JOIN memory_entities e ON e.id = m.entity_id AND e.valid_to IS NULL
         WHERE m.facet = 'invariant'
           AND NOT EXISTS (SELECT 1 FROM memory_code_anchor a WHERE a.entity_id = m.entity_id)
         ORDER BY m.entity_id",
    )
    .fetch_all(pool)
    .await
}

/// Transitive `is_a` ancestors of a concept (the deductive closure), nearest
/// first by name. `(ancestor_id, name)`.
pub async fn concept_ancestors(
    pool: &PgPool,
    entity_id: i64,
) -> Result<Vec<(i64, String)>, sqlx::Error> {
    sqlx::query_as(
        "WITH RECURSIVE anc(node) AS (
             SELECT to_entity_id FROM memory_relations
             WHERE from_entity_id = $1 AND relation_type = 'is_a' AND valid_to IS NULL
             UNION
             SELECT r.to_entity_id
             FROM anc a
             JOIN memory_relations r
               ON r.from_entity_id = a.node AND r.relation_type = 'is_a' AND r.valid_to IS NULL
         )
         SELECT a.node, e.name
         FROM anc a JOIN memory_entities e ON e.id = a.node AND e.valid_to IS NULL
         ORDER BY e.name",
    )
    .bind(entity_id)
    .fetch_all(pool)
    .await
}

/// Subtree under a concept: its hierarchy **descendants** (more-specific
/// concepts reachable by following `is_a`/`part_of`/`broader` edges in the
/// child→parent direction) within `max_depth` hops, as named edges. Correct over
/// the DAG — this is a recursive transitive closure; a materialized-path trie
/// would be ill-defined for multi-parent "diamond" concepts (see ADR-012), so
/// the recursive CTE is the right tool here.
pub async fn concept_descendants(
    pool: &PgPool,
    root_id: i64,
    max_depth: i32,
) -> Result<Vec<ConceptEdgeRow>, sqlx::Error> {
    sqlx::query_as(
        "WITH RECURSIVE sub(child_id, parent_id, relation, depth) AS (
             SELECT r.from_entity_id, r.to_entity_id, r.relation_type, 1
             FROM memory_relations r
             WHERE r.to_entity_id = $1
               AND r.relation_type IN ('is_a','part_of','broader') AND r.valid_to IS NULL
             UNION
             SELECT r.from_entity_id, r.to_entity_id, r.relation_type, s.depth + 1
             FROM sub s
             JOIN memory_relations r
               ON r.to_entity_id = s.child_id
               AND r.relation_type IN ('is_a','part_of','broader') AND r.valid_to IS NULL
             WHERE s.depth < $2
         )
         SELECT s.child_id, cf.name AS child_name, s.parent_id, pf.name AS parent_name, s.relation
         FROM sub s
         JOIN memory_entities cf ON cf.id = s.child_id AND cf.valid_to IS NULL
         JOIN memory_entities pf ON pf.id = s.parent_id AND pf.valid_to IS NULL
         ORDER BY s.parent_id, s.child_id",
    )
    .bind(root_id)
    .bind(max_depth)
    .fetch_all(pool)
    .await
}

/// All concepts for export: `(entity_id, name, facet, status)`.
pub async fn export_concepts(
    pool: &PgPool,
) -> Result<Vec<(i64, String, String, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT m.entity_id, e.name, m.facet, m.status
         FROM ontology_concept_meta m
         JOIN memory_entities e ON e.id = m.entity_id AND e.valid_to IS NULL
         ORDER BY m.entity_id",
    )
    .fetch_all(pool)
    .await
}

/// All ontology hierarchy/membership edges for export: `(from_id, to_id, relation)`.
pub async fn export_edges(pool: &PgPool) -> Result<Vec<(i64, i64, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT r.from_entity_id, r.to_entity_id, r.relation_type
         FROM memory_relations r
         JOIN ontology_concept_meta a ON a.entity_id = r.from_entity_id
         JOIN ontology_concept_meta b ON b.entity_id = r.to_entity_id
         WHERE r.relation_type IN ('is_a','part_of','broader','narrower','member_of')
           AND r.valid_to IS NULL
         ORDER BY r.from_entity_id, r.to_entity_id",
    )
    .fetch_all(pool)
    .await
}

/// `(child_id, parent_id)` for active `is_a` edges among one facet's concepts —
/// the Poincaré link-prediction input (Phase 8).
pub async fn load_isa_edge_ids(
    pool: &PgPool,
    facet: Facet,
) -> Result<Vec<(i64, i64)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT r.from_entity_id, r.to_entity_id
         FROM memory_relations r
         JOIN ontology_concept_meta cm ON cm.entity_id = r.from_entity_id AND cm.facet = $1
         JOIN ontology_concept_meta pm ON pm.entity_id = r.to_entity_id   AND pm.facet = $1
         WHERE r.relation_type = 'is_a' AND r.valid_to IS NULL",
    )
    .bind(facet.as_str())
    .fetch_all(pool)
    .await
}

/// `broader` candidate edges touching a concept (the `ontology_suggest_edges`
/// payload): `(from_id, from_name, to_id, to_name, weight)`.
pub async fn concept_broader_links(
    pool: &PgPool,
    entity_id: i64,
    limit: i64,
) -> Result<Vec<(i64, String, i64, String, f64)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT r.from_entity_id, ef.name, r.to_entity_id, et.name, r.importance::float8
         FROM memory_relations r
         JOIN memory_entities ef ON ef.id = r.from_entity_id AND ef.valid_to IS NULL
         JOIN memory_entities et ON et.id = r.to_entity_id   AND et.valid_to IS NULL
         WHERE r.relation_type = 'broader' AND r.valid_to IS NULL
           AND (r.from_entity_id = $1 OR r.to_entity_id = $1)
         ORDER BY r.importance DESC
         LIMIT $2",
    )
    .bind(entity_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// A named hierarchy edge between two concepts (for the `ontology_tree` view).
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ConceptEdgeRow {
    pub child_id: i64,
    pub child_name: String,
    pub parent_id: i64,
    pub parent_name: String,
    pub relation: String,
}

/// An evidence pointer row (for `ontology_concept`).
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ConceptEvidenceRow {
    pub evidence_kind: String,
    pub commit_id: Option<i64>,
    pub file_id: Option<i64>,
    pub mandate_ref: Option<String>,
    pub detail: Option<String>,
}

/// Active hierarchy edges (`is_a`/`part_of`/`broader`) among one facet's
/// concepts, with both endpoints' names — the `ontology_tree` payload.
pub async fn concept_hierarchy_edges(
    pool: &PgPool,
    facet: Facet,
) -> Result<Vec<ConceptEdgeRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT r.from_entity_id AS child_id, cf.name AS child_name,
                r.to_entity_id AS parent_id, pf.name AS parent_name, r.relation_type AS relation
         FROM memory_relations r
         JOIN ontology_concept_meta cm ON cm.entity_id = r.from_entity_id AND cm.facet = $1
         JOIN ontology_concept_meta pm ON pm.entity_id = r.to_entity_id   AND pm.facet = $1
         JOIN memory_entities cf ON cf.id = r.from_entity_id AND cf.valid_to IS NULL
         JOIN memory_entities pf ON pf.id = r.to_entity_id   AND pf.valid_to IS NULL
         WHERE r.relation_type IN ('is_a','part_of','broader') AND r.valid_to IS NULL
         ORDER BY r.from_entity_id, r.to_entity_id",
    )
    .bind(facet.as_str())
    .fetch_all(pool)
    .await
}

/// Resolve a concept reference (numeric id string or exact name) → entity id.
pub async fn resolve_concept(pool: &PgPool, name_or_id: &str) -> Result<Option<i64>, sqlx::Error> {
    if let Ok(id) = name_or_id.parse::<i64>() {
        let hit: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM memory_entities \
             WHERE id = $1 AND entity_type = 'concept' AND valid_to IS NULL",
        )
        .bind(id)
        .fetch_optional(pool)
        .await?;
        if hit.is_some() {
            return Ok(hit);
        }
    }
    sqlx::query_scalar(
        "SELECT id FROM memory_entities \
         WHERE name = $1 AND entity_type = 'concept' AND valid_to IS NULL ORDER BY id LIMIT 1",
    )
    .bind(name_or_id)
    .fetch_optional(pool)
    .await
}

/// Substring search over concept names **and invariant bodies**, optionally
/// facet-filtered. Matching `constraint_text` as well as `name` is what lets
/// "find the invariant about ambiguity" work — a concept surfaces if the query
/// appears in its name OR its constraint sentence (NULL bodies simply never
/// match the body leg, so non-invariant concepts are unaffected).
pub async fn search_concepts_by_name(
    pool: &PgPool,
    query: &str,
    facet: Option<Facet>,
    limit: i64,
) -> Result<Vec<ConceptBriefRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT e.id AS entity_id, e.name, m.facet, m.status, m.confidence
         FROM ontology_concept_meta m
         JOIN memory_entities e ON e.id = m.entity_id AND e.valid_to IS NULL
         WHERE (e.name ILIKE '%' || $1 || '%' OR m.constraint_text ILIKE '%' || $1 || '%')
           AND ($2::text IS NULL OR m.facet = $2)
         ORDER BY (m.status = 'canonical') DESC, m.confidence DESC, e.id
         LIMIT $3",
    )
    .bind(query)
    .bind(facet.map(|f| f.as_str()))
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Resolve a set of concept *names* to their brief rows by exact match — used to
/// expand fuzzy/prefix concept-trie candidates back to authoritative, live rows.
/// `valid_to IS NULL` drops stale trie entries; same-name concepts across
/// projects all surface. Optionally facet-filtered; ordered like
/// [`search_concepts_by_name`]. Empty `names` short-circuits to no rows.
pub async fn resolve_concepts_by_names(
    pool: &PgPool,
    names: &[String],
    facet: Option<Facet>,
    limit: i64,
) -> Result<Vec<ConceptBriefRow>, sqlx::Error> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as(
        "SELECT e.id AS entity_id, e.name, m.facet, m.status, m.confidence
         FROM ontology_concept_meta m
         JOIN memory_entities e ON e.id = m.entity_id AND e.valid_to IS NULL
         WHERE e.name = ANY($1)
           AND ($2::text IS NULL OR m.facet = $2)
         ORDER BY (m.status = 'canonical') DESC, m.confidence DESC, e.id
         LIMIT $3",
    )
    .bind(names)
    .bind(facet.map(|f| f.as_str()))
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Evidence rows backing a concept (for `ontology_concept`).
pub async fn list_concept_evidence(
    pool: &PgPool,
    entity_id: i64,
) -> Result<Vec<ConceptEvidenceRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT evidence_kind, commit_id, file_id, mandate_ref, detail
         FROM ontology_concept_evidence WHERE entity_id = $1 ORDER BY id",
    )
    .bind(entity_id)
    .fetch_all(pool)
    .await
}

/// Get-or-create a concept entity (by active name) + its facet metadata. The
/// `actor` sets provenance: an agent's concepts are `agent_write`, a user's are
/// `user_explicit`; the auto-miner's `auto_index` path is separate and never
/// clobbers these. Returns `(entity_id, created)`.
pub async fn create_concept(
    pool: &PgPool,
    name: &str,
    facet: Facet,
    actor: Actor,
) -> Result<(i64, bool), sqlx::Error> {
    let source = match actor {
        Actor::Agent => "agent_write",
        _ => "user_explicit",
    };
    let existing: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM memory_entities \
         WHERE name = $1 AND entity_type = 'concept' AND valid_to IS NULL LIMIT 1",
    )
    .bind(name)
    .fetch_optional(pool)
    .await?;
    let (entity_id, created) = match existing {
        Some(id) => (id, false),
        None => {
            let id: i64 = sqlx::query_scalar(
                "INSERT INTO memory_entities (name, entity_type, source) \
                 VALUES ($1, 'concept', $2::memory_source) RETURNING id",
            )
            .bind(name)
            .bind(source)
            .fetch_one(pool)
            .await?;
            (id, true)
        }
    };
    let build_method = match actor {
        Actor::Agent => "agent",
        _ => "user",
    };
    upsert_concept_meta(pool, entity_id, facet, build_method, None).await?;
    Ok((entity_id, created))
}

/// Agent-authored invariant assertion: create the concept, set its invariant
/// metadata (always `status='candidate'` — an agent CANNOT self-canonicalize),
/// attach `kind='agent'` evidence, and (optionally) anchor it to a file. Returns
/// the concept entity id.
pub async fn agent_assert_invariant(
    pool: &PgPool,
    name: &str,
    constraint_text: &str,
    rationale: &str,
    file_id: Option<i64>,
) -> Result<i64, sqlx::Error> {
    let (entity_id, _) = create_concept(pool, name, Facet::Invariant, Actor::Agent).await?;
    upsert_invariant_meta(pool, entity_id, constraint_text, rationale, "agent", None).await?;
    let provenance_key = format!("agent:{entity_id}:{name}");
    insert_concept_evidence(
        pool,
        entity_id,
        EvidenceKind::Agent,
        None,
        file_id,
        None,
        Some(constraint_text),
        &provenance_key,
    )
    .await?;
    if let Some(fid) = file_id {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM memory_code_anchor \
             WHERE entity_id = $1 AND file_id = $2 AND anchor_type = 'concept_code')",
        )
        .bind(entity_id)
        .bind(fid)
        .fetch_one(pool)
        .await?;
        if !exists {
            crate::db::queries::memory_anchor_entity(
                pool,
                entity_id,
                Some(fid),
                None,
                None,
                None,
                None,
                "concept_code",
            )
            .await?;
        }
    }
    Ok(entity_id)
}

/// Same-facet concept pairs whose observation embeddings are within cosine
/// distance `1 - tau` (i.e. cosine similarity ≥ `tau`) — the EDC canonicalization
/// candidate set (Phase 5). One embedding per concept (its lowest-id embedded
/// observation). Returns `(lower_id, higher_id, cosine)` with `lower_id < higher_id`.
pub async fn find_similar_concept_pairs(
    pool: &PgPool,
    facet: Facet,
    tau: f64,
) -> Result<Vec<(i64, i64, f64)>, sqlx::Error> {
    sqlx::query_as(
        "WITH ce AS (
             SELECT DISTINCT ON (m.entity_id) m.entity_id, o.embedding
             FROM ontology_concept_meta m
             JOIN memory_entities e     ON e.id = m.entity_id AND e.valid_to IS NULL
             JOIN memory_observations o ON o.entity_id = m.entity_id AND o.embedding IS NOT NULL
             WHERE m.facet = $1
             ORDER BY m.entity_id, o.id
         )
         SELECT a.entity_id, b.entity_id, (1 - (a.embedding <=> b.embedding))::float8
         FROM ce a JOIN ce b ON a.entity_id < b.entity_id
         WHERE (1 - (a.embedding <=> b.embedding)) >= $2
         ORDER BY a.entity_id, b.entity_id",
    )
    .bind(facet.as_str())
    .bind(tau)
    .fetch_all(pool)
    .await
}

/// `(entity_id, effect)` rows: the shadow-ASR effects exhibited (above
/// `min_support` symbol occurrences) by the code units of each facet concept,
/// via its `concept_topic` anchor → chunks → files → symbols → effects. The
/// FCA attribute basis (Phase 4).
pub async fn load_concept_effect_rows(
    pool: &PgPool,
    facet: Facet,
    min_support: i64,
) -> Result<Vec<(i64, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT m.entity_id, se.effect
         FROM ontology_concept_meta m
         JOIN memory_code_anchor a        ON a.entity_id = m.entity_id AND a.topic_id IS NOT NULL
         JOIN chunk_topic_assignments cta ON cta.topic_id = a.topic_id AND cta.membership_score >= 0.05
         JOIN file_chunks fc              ON fc.id = cta.chunk_id
         JOIN file_symbols fs             ON fs.file_id = fc.file_id
         JOIN symbol_effects se           ON se.symbol_id = fs.id
         WHERE m.facet = $1
         GROUP BY m.entity_id, se.effect
         HAVING COUNT(*) >= $2
         ORDER BY m.entity_id, se.effect",
    )
    .bind(facet.as_str())
    .bind(min_support)
    .fetch_all(pool)
    .await
}
