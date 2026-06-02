//! `ontology-link-predict` cron (Phase 8, optional ML): Poincaré-embedding
//! missing-edge prediction over the per-facet `is_a` DAG.
//!
//! For each facet with enough structure, embeds the `is_a` graph into the
//! Poincaré ball ([`crate::ontology::embed_hyperbolic`]) and proposes
//! hyperbolically-close-but-unlinked, norm-ordered pairs as **soft `broader`
//! candidate** edges (low importance = model confidence). They are deliberately
//! NOT inserted as `is_a` — the deterministic FCA backbone stays authoritative;
//! these are suggestions surfaced by `ontology_suggest_edges` for a curator. The
//! strict norm ordering keeps the proposed set acyclic. Opt-in
//! (`[ontology] link_predict_enabled`, default off); deterministic (seed 42).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use sqlx::PgPool;
use tracing::{info, warn};

use crate::config::OntologyConfig;
use crate::db::queries;
use crate::ontology::edge::OntologyRelation;
use crate::ontology::embed_hyperbolic;
use crate::ontology::facet::Facet;

const DIM: usize = 8;
const EPOCHS: usize = 300;
const LR: f64 = 0.2;
const NEG_K: usize = 5;
const SEED: u64 = 42;
const MAX_DIST: f64 = 8.0;
const TOP_K: usize = 32;

/// Predict + persist soft `broader` candidate edges per facet.
pub async fn run_ontology_link_predict(pool: &PgPool) -> Result<(), sqlx::Error> {
    let mut total = 0usize;
    for facet in Facet::ALL {
        let ids = queries::list_concept_ids_by_facet(pool, *facet).await?;
        if ids.len() < 4 {
            continue;
        }
        let edges = queries::load_isa_edge_ids(pool, *facet).await?;
        if edges.len() < 2 {
            continue;
        }
        let index: HashMap<i64, usize> = ids.iter().enumerate().map(|(i, &id)| (id, i)).collect();
        let local: Vec<(usize, usize)> = edges
            .iter()
            .filter_map(|(c, p)| Some((*index.get(c)?, *index.get(p)?)))
            .collect();
        if local.len() < 2 {
            continue;
        }
        let model = embed_hyperbolic::train(ids.len(), &local, DIM, EPOCHS, LR, NEG_K, SEED);
        let existing: HashSet<(usize, usize)> = local.iter().copied().collect();
        for (c, p, dist) in model.predict_missing(&existing, MAX_DIST, TOP_K) {
            // Low, distance-decayed confidence (kept well below FCA's 1.0 is_a weight).
            let confidence = (1.0 / (1.0 + dist)).clamp(0.0, 0.5);
            match queries::insert_ontology_edge(
                pool,
                ids[c],
                ids[p],
                OntologyRelation::Broader,
                confidence,
            )
            .await
            {
                Ok(true) => total += 1,
                Ok(false) => {}
                Err(e) => warn!(error = %e, facet = facet.as_str(), "predicted edge insert failed"),
            }
        }
    }
    queries::refresh_memory_unified_edges(pool).await?;
    info!(
        predicted_broader_edges = total,
        "ontology-link-predict complete"
    );
    Ok(())
}

/// Cron entry point: run the prediction pass, logging (not panicking) on error.
pub async fn run_or_log(pool: Arc<PgPool>, _config: OntologyConfig) {
    if let Err(e) = run_ontology_link_predict(&pool).await {
        warn!(error = %e, "ontology-link-predict pass failed");
    }
}
