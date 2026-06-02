//! `ontology-integrate` cron (Phase 11): wire analyzers in as ontology
//! **producers** — attach their findings as `evidence_kind='finding'` to the
//! concepts governing the same code.
//!
//! Representative integration: the concurrency subsystem (v22). A
//! `concurrency_findings` row touching a file/symbol that an ontology concept is
//! anchored to becomes a `finding` evidence row on that concept (idempotent via
//! `provenance_key`). The same adapter pattern extends to the security / metrics
//! / prediction finding tables and to Leiden `community_detection` (→ component
//! concepts) — each a join over shared code anchors + an evidence insert.

use std::sync::Arc;

use sqlx::PgPool;
use tracing::{info, warn};

use crate::config::OntologyConfig;
use crate::db::queries;
use crate::ontology::edge::EvidenceKind;

/// Attach concurrency findings (v22) as evidence to the concepts that govern the
/// same files/symbols. Idempotent (`provenance_key`-deduped).
pub async fn run_ontology_integrate(pool: &PgPool) -> Result<(), sqlx::Error> {
    let rows = queries::concept_concurrency_findings(pool).await?;
    let mut attached = 0usize;
    for (entity_id, finding_id, title, file_id) in rows {
        let provenance_key = format!("finding:concurrency:{finding_id}:{entity_id}");
        if queries::insert_concept_evidence(
            pool,
            entity_id,
            EvidenceKind::Finding,
            None,
            file_id,
            None,
            Some(&title),
            &provenance_key,
        )
        .await?
        {
            attached += 1;
        }
    }
    info!(finding_evidence_attached = attached, "ontology-integrate complete");
    Ok(())
}

/// Cron entry point: run the integration, logging (not panicking) on error.
pub async fn run_or_log(pool: Arc<PgPool>, _config: OntologyConfig) {
    if let Err(e) = run_ontology_integrate(&pool).await {
        warn!(error = %e, "ontology-integrate pass failed");
    }
}
