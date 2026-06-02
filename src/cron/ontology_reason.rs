//! `ontology-reason` cron (Phase 9): periodic deductive constraint check over
//! the ontology (is_a acyclicity + invariants-must-anchor), logging a summary.
//! The on-demand detail is served by the `ontology_check` tool. Opt-in
//! (`[ontology] reasoning_enabled`, default off).

use std::sync::Arc;

use sqlx::PgPool;
use tracing::{info, warn};

use crate::config::OntologyConfig;
use crate::ontology::reason;

/// Run the constraint check and log a summary.
pub async fn run_ontology_reason(pool: &PgPool) -> Result<(), sqlx::Error> {
    let violations = reason::check_constraints(pool).await?;
    let cycles = violations.iter().filter(|v| v.kind == "is_a_cycle").count();
    let unanchored = violations
        .iter()
        .filter(|v| v.kind == "unanchored_invariant")
        .count();
    info!(
        is_a_cycles = cycles,
        unanchored_invariants = unanchored,
        total_violations = violations.len(),
        "ontology-reason constraint check complete"
    );
    Ok(())
}

/// Cron entry point: run the check, logging (not panicking) on error.
pub async fn run_or_log(pool: Arc<PgPool>, _config: OntologyConfig) {
    if let Err(e) = run_ontology_reason(&pool).await {
        warn!(error = %e, "ontology-reason pass failed");
    }
}
