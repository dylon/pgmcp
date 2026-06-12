//! Migration step 34: `external_scanner_findings_v1` — persistence for the
//! `security_scan` subsystem (`src/cron/security_scan.rs`), which runs installed
//! external security scanners (gitleaks, semgrep, trivy, cargo-audit, …) over
//! each indexed project and records their findings.
//!
//! Three additive tables (none touching the `work_items` spine):
//!
//! - `external_scanner_runs` — one row per (project, scanner) invocation: an
//!   audit trail of `status` (ok/timeout/error/absent/skipped), exit code,
//!   duration, tool version, and finding count.
//! - `external_scanner_findings` — one row per finding. `fingerprint` (UNIQUE)
//!   makes re-scans idempotent (refresh `last_seen_at`); a finding not re-seen in
//!   a later run flips `status='resolved'`. `provenance_key` (UNIQUE) is the
//!   idempotency lineage shared with `work_item_finding_provenance` when the
//!   finding is promoted to a `pending` bug. `severity` CHECK is built from the
//!   tracker [`Severity`](crate::tracker::severity::Severity) enum (ADR-003).
//! - `external_scanner_sbom` — the SBOM artifact `syft` produces per project.
//!
//! Also widens `work_item_finding_provenance.finding_source`'s CHECK to admit the
//! new `security_scan` source (re-installed from the current `FindingSource`
//! vocabulary via the stamp-aware `install_check`; the v17 step froze the prior
//! four-value list, so this DROP+ADDs it — ADR-003).
//!
//! Version-gated (runs once); every statement is `IF NOT EXISTS` / idempotent.

use sqlx::PgPool;

use crate::tracker::severity;

pub(super) const EXTERNAL_SCANNER_FINDINGS_V1: i32 = 34;
pub(super) const EXTERNAL_SCANNER_FINDINGS_V1_NAME: &str = "external_scanner_findings_v1";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    // 1. Per-invocation audit trail: one row per (project, scanner) run.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS external_scanner_runs (
            id             BIGSERIAL PRIMARY KEY,
            project_id     INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            scanner        TEXT NOT NULL,
            status         TEXT NOT NULL CHECK (status IN ('ok','timeout','error','absent','skipped')),
            exit_code      INTEGER,
            duration_ms    BIGINT,
            findings_count INTEGER NOT NULL DEFAULT 0,
            tool_version   TEXT,
            detail         TEXT,
            started_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
            finished_at    TIMESTAMPTZ
        )",
    )
    .execute(pool)
    .await?;

    // 2. The findings. `severity` CHECK sourced from the tracker Severity enum so
    //    a promoted finding's severity is exactly a valid `work_items.severity`.
    let create_findings = format!(
        "CREATE TABLE IF NOT EXISTS external_scanner_findings (
            id             BIGSERIAL PRIMARY KEY,
            project_id     INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            run_id         BIGINT REFERENCES external_scanner_runs(id) ON DELETE SET NULL,
            scanner        TEXT NOT NULL,
            rule_id        TEXT,
            severity       TEXT NOT NULL CHECK (severity IN ({severities})),
            file_path      TEXT,
            line           INTEGER,
            title          TEXT NOT NULL,
            message        TEXT,
            raw            JSONB,
            fingerprint    TEXT NOT NULL UNIQUE,
            provenance_key TEXT NOT NULL UNIQUE,
            status         TEXT NOT NULL DEFAULT 'open' CHECK (status IN ('open','resolved')),
            first_seen_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
            last_seen_at   TIMESTAMPTZ NOT NULL DEFAULT now()
        )",
        severities = severity::sql_in_list(),
    );
    sqlx::query(&create_findings).execute(pool).await?;

    // 3. SBOM artifacts (syft). One current SBOM per (project, scanner, format).
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS external_scanner_sbom (
            id           BIGSERIAL PRIMARY KEY,
            project_id   INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            scanner      TEXT NOT NULL DEFAULT 'syft',
            format       TEXT NOT NULL,
            sbom         JSONB NOT NULL,
            generated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
            UNIQUE (project_id, scanner, format)
        )",
    )
    .execute(pool)
    .await?;

    for idx in [
        "CREATE INDEX IF NOT EXISTS idx_ext_scan_runs_project \
            ON external_scanner_runs(project_id)",
        "CREATE INDEX IF NOT EXISTS idx_ext_scan_findings_project_sev \
            ON external_scanner_findings(project_id, severity)",
        "CREATE INDEX IF NOT EXISTS idx_ext_scan_findings_scanner \
            ON external_scanner_findings(scanner)",
        "CREATE INDEX IF NOT EXISTS idx_ext_scan_findings_status \
            ON external_scanner_findings(status)",
    ] {
        sqlx::query(idx).execute(pool).await?;
    }

    // 4. Widen the finding_source CHECK to admit `security_scan`. The v17 step
    //    installed it from the then-current FindingSource vocabulary; this
    //    re-installs from the current one (`ensure_named_constraint` DROP+ADDs
    //    when the stamped definition differs). Idempotent.
    super::v4_work_items::install_check(
        pool,
        "work_item_finding_provenance",
        "work_item_finding_provenance_source_check",
        &format!(
            "finding_source IN ({})",
            crate::tracker::git_link::finding_source_sql_in_list()
        ),
    )
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        assert_eq!(EXTERNAL_SCANNER_FINDINGS_V1, 34);
        assert_eq!(
            EXTERNAL_SCANNER_FINDINGS_V1_NAME,
            "external_scanner_findings_v1"
        );
    }
}
