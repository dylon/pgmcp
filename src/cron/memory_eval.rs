//! Memory-server Phase 9: invariant-scan cron.
//!
//! The integration scenarios in `pgmcp-testing/tests/memory_eval.rs`
//! cover end-to-end recall/contradiction/multi-hop/forget/reflection
//! behaviour; they run under `cargo test` (and therefore
//! `scripts/verify.sh`). This cron is the complementary on-prem check:
//! when enabled it periodically scans the live memory graph for
//! bi-temporal / supersession / referential-integrity violations and
//! persists the report into `pgmcp_metadata` under
//! `memory_eval_last_report`.
//!
//! Default is **off** (`[memory.eval] cron_enabled = false`) — the cron
//! is opportunistic regression detection, not a verification gate.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use sqlx::PgPool;
use tracing::{info, warn};

use crate::db::queries;
use crate::stats::tracker::StatsTracker;

pub async fn run_or_log(pool: Arc<PgPool>, stats: Arc<StatsTracker>, row_cap: i64) {
    let _ = stats.cron_executions.fetch_add(1, Ordering::Relaxed);
    match queries::memory_eval_invariants(&pool, row_cap).await {
        Ok(report) => {
            let total_violations = report.entities_temporal_invalid
                + report.observations_temporal_invalid
                + report.relations_temporal_invalid
                + report.entity_supersede_cycles
                + report.observation_supersede_cycles
                + report.relation_supersede_cycles
                + report.orphan_observations
                + report.reflection_derived_from_missing
                + report.stale_code_anchors
                + report.forget_log_dangling;
            stats.memory_eval_runs.fetch_add(1, Ordering::Relaxed);
            stats
                .memory_eval_invariant_violations
                .fetch_add(total_violations as u64, Ordering::Relaxed);
            if let Err(e) = queries::record_memory_eval_report(&pool, &report).await {
                stats.cron_panics.fetch_add(1, Ordering::Relaxed);
                warn!(error = %e, "memory-eval cron: failed to persist report");
            } else if total_violations > 0 {
                warn!(
                    violations = total_violations,
                    entities_temporal_invalid = report.entities_temporal_invalid,
                    observations_temporal_invalid = report.observations_temporal_invalid,
                    relations_temporal_invalid = report.relations_temporal_invalid,
                    entity_supersede_cycles = report.entity_supersede_cycles,
                    observation_supersede_cycles = report.observation_supersede_cycles,
                    relation_supersede_cycles = report.relation_supersede_cycles,
                    orphan_observations = report.orphan_observations,
                    reflection_derived_from_missing = report.reflection_derived_from_missing,
                    stale_code_anchors = report.stale_code_anchors,
                    forget_log_dangling = report.forget_log_dangling,
                    "memory-eval cron: invariant violations detected"
                );
            } else {
                info!("memory-eval cron: all invariants clean");
            }
        }
        Err(e) => {
            stats.cron_panics.fetch_add(1, Ordering::Relaxed);
            warn!(error = %e, "memory-eval cron: scan failed");
        }
    }
}
