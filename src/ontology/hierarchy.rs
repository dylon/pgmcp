//! Per-facet bottom-up hierarchy construction (Phase 4).
//!
//! Builds the `is_a` Hasse cover among a facet's concepts from their shadow-ASR
//! effect attributes (the FCA backbone, [`crate::ontology::fca`]). Single-aspect
//! by construction: each facet is built independently, so an `is_a` edge never
//! crosses facets.

use std::collections::{BTreeSet, HashMap};

use sqlx::PgPool;

use crate::db::queries;
use crate::ontology::edge::OntologyRelation;
use crate::ontology::facet::Facet;
use crate::ontology::fca::{self, ConceptAttrs};

/// Insert the `is_a` Hasse cover for the given concept-attribute descriptions.
/// Idempotent (the edge insert is existence-guarded). Returns new-edge count.
pub async fn build_isa_from_attrs(
    pool: &PgPool,
    concepts: &[ConceptAttrs],
) -> Result<usize, sqlx::Error> {
    let mut inserted = 0usize;
    for (child, parent) in fca::is_a_cover(concepts) {
        if queries::insert_ontology_edge(pool, child, parent, OntologyRelation::IsA, 1.0).await? {
            inserted += 1;
        }
    }
    Ok(inserted)
}

/// Build the `is_a` hierarchy for one facet. Every active facet concept
/// participates: those whose code units exhibit no effects become the
/// most-general nodes (empty attribute set). Returns new-edge count.
pub async fn build_facet_isa(
    pool: &PgPool,
    facet: Facet,
    min_support: i64,
) -> Result<usize, sqlx::Error> {
    let ids = queries::list_concept_ids_by_facet(pool, facet).await?;
    if ids.len() < 2 {
        return Ok(0);
    }
    let rows = queries::load_concept_effect_rows(pool, facet, min_support).await?;

    // Intern effect strings → u32 attribute ids, bucketed per concept. Every
    // concept id starts with an empty set so attribute-less concepts are the
    // poset's most-general nodes.
    let mut vocab: HashMap<String, u32> = HashMap::new();
    let mut attrs: HashMap<i64, BTreeSet<u32>> =
        ids.iter().map(|&id| (id, BTreeSet::new())).collect();
    for (entity_id, effect) in rows {
        let next = vocab.len() as u32;
        let aid = *vocab.entry(effect).or_insert(next);
        attrs.entry(entity_id).or_default().insert(aid);
    }

    let concepts: Vec<ConceptAttrs> = ids
        .into_iter()
        .map(|id| ConceptAttrs {
            entity_id: id,
            attrs: attrs.remove(&id).unwrap_or_default(),
        })
        .collect();
    build_isa_from_attrs(pool, &concepts).await
}

/// EDC canonicalization (Phase 5): link same-facet near-duplicate concepts
/// (observation-embedding cosine ≥ `tau`) with a `broader` edge from the variant
/// (higher entity id) to the canonical (lower entity id), weighted by the cosine.
/// Deterministic candidate generation; the Phase-9 egglog pass does true merging.
/// Idempotent (edge insert is existence-guarded). Returns new-edge count.
pub async fn build_broader_edges(
    pool: &PgPool,
    facet: Facet,
    tau: f64,
) -> Result<usize, sqlx::Error> {
    let pairs = queries::find_similar_concept_pairs(pool, facet, tau).await?;
    let mut inserted = 0usize;
    for (canonical, variant, cosine) in pairs {
        // `variant broader canonical` ⇒ canonical (lower id) is the broader concept.
        if queries::insert_ontology_edge(
            pool,
            variant,
            canonical,
            OntologyRelation::Broader,
            cosine,
        )
        .await?
        {
            inserted += 1;
        }
    }
    Ok(inserted)
}
