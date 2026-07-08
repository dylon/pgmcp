//! Migration step 69: widen `cron_run_history_skip_reason_check` to admit the
//! `memory_pressure` skip reason (the new `src/health` memory-watchdog gate).
//!
//! The v40 migration installed the skip_reason CHECK from
//! `vocab::skip_reason_sql_in_list()`, but a numbered migration step runs EXACTLY
//! ONCE (`apply_step` short-circuits on `version_applied`), so an install that
//! already recorded v40 never re-runs it — adding a `SkipReason` variant does NOT
//! re-widen the stamped constraint on those installs. This step re-installs the
//! CHECK from the (now-wider) vocabulary; `ensure_named_constraint` sees a
//! different stamp and DROPs + re-adds the constraint. Widening an allowed-value
//! set always re-validates cleanly (no existing row can violate a superset).
//! Fresh installs already receive the widened base constraint via v40's
//! vocabulary build. Mirrors `v68_experiment_audit_action`.

use sqlx::PgPool;

pub(super) const MEMORY_PRESSURE_SKIP_REASON: i32 = 69;
pub(super) const MEMORY_PRESSURE_SKIP_REASON_NAME: &str = "memory_pressure_skip_reason";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    // Build the predicate exactly as v40 does, so the stamped definition matches
    // a fresh install and the closed vocabulary stays the single source of truth.
    let predicate = format!(
        "skip_reason IS NULL OR skip_reason IN ({})",
        crate::cron::history::vocab::skip_reason_sql_in_list()
    );
    super::v4_work_items::install_check(
        pool,
        "cron_run_history",
        "cron_run_history_skip_reason_check",
        &predicate,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        assert_eq!(MEMORY_PRESSURE_SKIP_REASON, 69);
        assert_eq!(
            MEMORY_PRESSURE_SKIP_REASON_NAME,
            "memory_pressure_skip_reason"
        );
    }

    #[test]
    fn predicate_includes_memory_pressure() {
        // The whole point of this step: the widened vocabulary admits the new
        // memory_pressure skip reason.
        assert!(
            crate::cron::history::vocab::skip_reason_sql_in_list().contains("memory_pressure"),
            "v69 must widen the CHECK to include memory_pressure"
        );
    }
}
