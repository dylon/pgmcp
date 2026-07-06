//! Migration step 68: widen `webui_audit_log_action_check` to admit the
//! `experiment_update` audit action (ADR-034 admin-console amendment —
//! experiment↔project assignment).
//!
//! The v66 migration installed the `action` CHECK from
//! `AuditAction::sql_in_list()`, but a numbered migration step runs EXACTLY
//! ONCE (`apply_step` short-circuits on `version_applied`), so an install that
//! already recorded v66 never re-runs it — adding an enum variant does NOT
//! re-widen the stamped constraint on those installs. This step re-installs the
//! CHECK from the (now-wider) `AuditAction::sql_in_list()`; the
//! `ensure_named_constraint` stamp differs from the v66 stamp, so it DROPs +
//! re-adds the constraint. Fresh installs already receive the widened base
//! constraint via v66's every-vocabulary build. Mirrors `v65_global_mandates`.

use sqlx::PgPool;

pub(super) const EXPERIMENT_AUDIT_ACTION: i32 = 68;
pub(super) const EXPERIMENT_AUDIT_ACTION_NAME: &str = "experiment_audit_action";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    // Build the predicate exactly as v66 does, so the stamped definition matches
    // a fresh install and the closed vocabulary stays the single source of truth.
    let predicate = format!(
        "action IN ({})",
        crate::api::audit::AuditAction::sql_in_list()
    );
    super::v4_work_items::install_check(
        pool,
        "webui_audit_log",
        "webui_audit_log_action_check",
        &predicate,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        assert_eq!(EXPERIMENT_AUDIT_ACTION, 68);
        assert_eq!(EXPERIMENT_AUDIT_ACTION_NAME, "experiment_audit_action");
    }

    #[test]
    fn predicate_includes_experiment_update() {
        // The whole point of this step: the widened vocabulary admits the new
        // experiment_update action.
        assert!(
            crate::api::audit::AuditAction::sql_in_list().contains("experiment_update"),
            "v68 must widen the CHECK to include experiment_update"
        );
    }
}
