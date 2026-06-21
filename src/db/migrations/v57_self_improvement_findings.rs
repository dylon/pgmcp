//! Migration step 57: `self_improvement_findings_v1` — admits the
//! `self_improvement` source into the work-item finding-provenance vocabulary.
//!
//! The `self_improvement` discovery cron (`src/cron/self_improvement.rs`, ADR-015)
//! promotes recurring agent-outcome failure clusters / persistently low-trust
//! approaches into `pending` `idea` proposals via the shared
//! [`crate::db::queries::promote_finding`] path, which writes
//! `work_item_finding_provenance.finding_source`. That column carries a CHECK
//! built from the closed [`crate::tracker::git_link::FindingSource`] enum; this
//! step re-installs it from the now-current vocabulary (the stamp-aware
//! `install_check` DROP+ADDs only when the stamped definition differs — the v34
//! idiom). Additive + idempotent; no data migration.

use sqlx::PgPool;

pub(super) const SELF_IMPROVEMENT_FINDINGS_V1: i32 = 57;
pub(super) const SELF_IMPROVEMENT_FINDINGS_V1_NAME: &str = "self_improvement_findings_v1";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    // Re-install the finding_source CHECK from the current FindingSource
    // vocabulary (now including `self_improvement`). `install_check` is
    // stamp-aware, so this is idempotent.
    super::v4_work_items::install_check(
        pool,
        "work_item_finding_provenance",
        "work_item_finding_provenance_source_check",
        &format!(
            "finding_source IN ({})",
            crate::tracker::git_link::finding_source_sql_in_list()
        ),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        assert_eq!(SELF_IMPROVEMENT_FINDINGS_V1, 57);
        assert_eq!(
            SELF_IMPROVEMENT_FINDINGS_V1_NAME,
            "self_improvement_findings_v1"
        );
    }
}
