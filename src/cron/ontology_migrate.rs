//! `ontology-migrate` cron (Phase 10): fold the curated software-pattern catalog
//! into the ontology so it is one concept hierarchy rather than a parallel table.
//!
//! Each `software_patterns.category` becomes a `paradigm` concept; each pattern
//! becomes a `design_pattern` concept `is_a` its paradigm, with the catalog
//! `kind`/`slug` preserved as concept attributes. Imported concepts are
//! `source='migration'`, `build_method='pattern_catalog'`, and promoted to
//! `canonical` (a curated catalog) via `Actor::System` — never an agent. Fully
//! idempotent: get-or-create entities, curation-safe meta, guarded edges, upsert
//! attrs. (Topics already bridge to concepts via the concept cron; effect/
//! type_tag/protocol are already first-class graph node types.)

use std::collections::HashMap;
use std::sync::Arc;

use sqlx::PgPool;
use tracing::{info, warn};

use crate::config::OntologyConfig;
use crate::db::queries;
use crate::ontology::edge::OntologyRelation;
use crate::ontology::facet::{ConceptStatus, Facet};
use crate::tracker::transition::Actor;

/// Migrate the software-pattern catalog into ontology concepts + `is_a` edges.
pub async fn run_ontology_migrate(pool: &PgPool) -> Result<(), sqlx::Error> {
    let patterns = queries::load_software_patterns(pool).await?;
    let mut paradigms: HashMap<String, i64> = HashMap::new();
    let mut migrated = 0usize;

    for (slug, name, category, kind) in patterns {
        // Paradigm (family) concept — one per distinct category.
        let paradigm_id = match paradigms.get(&category) {
            Some(&id) => id,
            None => {
                let (id, _) =
                    queries::migrate_concept(pool, &category, Facet::Paradigm, "pattern_catalog")
                        .await?;
                let _ =
                    queries::set_concept_status(pool, id, ConceptStatus::Canonical, Actor::System)
                        .await;
                paradigms.insert(category.clone(), id);
                id
            }
        };

        // Pattern concept `is_a` its paradigm, with catalog provenance as attrs.
        let (concept_id, created) =
            queries::migrate_concept(pool, &name, Facet::DesignPattern, "pattern_catalog").await?;
        if created {
            migrated += 1;
        }
        let _ =
            queries::set_concept_status(pool, concept_id, ConceptStatus::Canonical, Actor::System)
                .await;
        queries::insert_ontology_edge(pool, concept_id, paradigm_id, OntologyRelation::IsA, 1.0)
            .await?;
        queries::set_concept_attr(pool, concept_id, "pattern_kind", &kind).await?;
        queries::set_concept_attr(pool, concept_id, "pattern_slug", &slug).await?;
    }

    queries::refresh_memory_unified_nodes(pool).await?;
    queries::refresh_memory_unified_edges(pool).await?;
    info!(
        migrated_patterns = migrated,
        paradigms = paradigms.len(),
        "ontology-migrate pattern catalog complete"
    );
    Ok(())
}

/// Cron entry point: run the migration, logging (not panicking) on error.
pub async fn run_or_log(pool: Arc<PgPool>, _config: OntologyConfig) {
    if let Err(e) = run_ontology_migrate(&pool).await {
        warn!(error = %e, "ontology-migrate pass failed");
    }
}
