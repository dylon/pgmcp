//! `ontology-build` cron (Phase 4): per-facet bottom-up `is_a` hierarchy.
//!
//! For each facet, derives the `is_a` Hasse cover from concepts' shadow-ASR
//! effect attributes (FCA) and inserts the edges into `memory_relations` (whence
//! they surface in the unified-graph edge matview). Deterministic + idempotent;
//! runs after the invariant-mining + concept-seeding crons so the concepts it
//! orders already carry facet metadata.

use std::sync::Arc;

use sqlx::PgPool;
use tracing::{info, warn};

use crate::config::OntologyConfig;
use crate::db::queries;
use crate::ontology::facet::Facet;
use crate::ontology::hierarchy;

/// Minimum number of symbols a concept's topic must exhibit an effect on for that
/// effect to count as a concept attribute (the FCA support / iceberg threshold).
const MIN_EFFECT_SUPPORT: i64 = 2;

/// Build the `is_a` hierarchy across all facets, then refresh the edge matview.
pub async fn run_ontology_build(pool: &PgPool) -> Result<(), sqlx::Error> {
    let mut total = 0usize;
    for facet in Facet::ALL {
        match hierarchy::build_facet_isa(pool, *facet, MIN_EFFECT_SUPPORT).await {
            Ok(n) => total += n,
            Err(e) => warn!(error = %e, facet = facet.as_str(), "facet is_a build failed"),
        }
    }
    queries::refresh_memory_unified_edges(pool).await?;
    info!(is_a_edges = total, "ontology-build is_a hierarchy complete");
    Ok(())
}

/// Cron entry point: run the build, logging (not panicking) on error.
pub async fn run_or_log(pool: Arc<PgPool>, _config: OntologyConfig) {
    if let Err(e) = run_ontology_build(&pool).await {
        warn!(error = %e, "ontology-build pass failed");
    }
}
