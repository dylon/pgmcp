//! Migration step 58: `lint_finding_class_v1` — adds `finding_class` to
//! `external_scanner_findings` so the crucible linter loop (ADR-014, E7) can
//! persist lint diagnostics via `POST /api/scanner/findings` without their
//! masquerading as security vulnerabilities.
//!
//! Default `'security'` (every existing finding + every `security_scan` finding
//! stays a security finding, so the `security_scan` read path is unchanged);
//! `'lint'` is the new class, filtered out of `security_scan` by default. Closed
//! two-value vocabulary installed via the stamp-aware `install_check` (ADR-003).
//! Additive + idempotent.

use sqlx::PgPool;

pub(super) const LINT_FINDING_CLASS_V1: i32 = 58;
pub(super) const LINT_FINDING_CLASS_V1_NAME: &str = "lint_finding_class_v1";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "ALTER TABLE external_scanner_findings
            ADD COLUMN IF NOT EXISTS finding_class TEXT NOT NULL DEFAULT 'security'",
    )
    .execute(pool)
    .await?;
    super::v4_work_items::install_check(
        pool,
        "external_scanner_findings",
        "external_scanner_findings_class_check",
        "finding_class IN ('security','lint')",
    )
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_ext_scan_findings_class
            ON external_scanner_findings(finding_class)",
    )
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        assert_eq!(LINT_FINDING_CLASS_V1, 58);
        assert_eq!(LINT_FINDING_CLASS_V1_NAME, "lint_finding_class_v1");
    }
}
