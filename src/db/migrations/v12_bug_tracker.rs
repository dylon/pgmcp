//! Migration step 12: `bug_tracker_v1`.
//!
//! Turns the general-purpose work-item tracker into a first-class bug tracker:
//!
//! - a nullable `severity` column on the `work_items` spine (the *impact* axis,
//!   orthogonal to `priority` = *urgency*), CHECK-constrained from the closed
//!   [`crate::tracker::severity::Severity`] enum;
//! - a 1:1 `work_item_bug_details` sidecar holding the structured bug report
//!   (reproduction / expected-vs-actual / environment / affected & fixed
//!   version / root cause / regression flag / triage + resolution), kept off the
//!   hot 30-column spine exactly like `acceptance_criteria` / `work_item_code_anchor`;
//! - the `triage` / `confirmed` lifecycle states, which land in the kind/status
//!   CHECK vocabularies via the closed-enum-driven `install_work_items_checks`
//!   reconcile.
//!
//! See ADR-004 and the plan `~/.claude/plans/how-extensive-is-the-zazzy-galaxy.md`.

use sqlx::PgPool;

pub(super) const BUG_TRACKER_V1: i32 = 12;
pub(super) const BUG_TRACKER_V1_NAME: &str = "bug_tracker_v1";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    // 1. Severity (impact) column on the spine — nullable; only `kind='bug'`
    //    items carry it. `ADD COLUMN IF NOT EXISTS` with no default is an instant
    //    metadata-only change (no table rewrite, no backfill); the 14 non-bug
    //    kinds simply keep it NULL. The CHECK is installed by the closed-enum
    //    reconcile (`install_work_items_checks`) called at the end of this step.
    sqlx::query("ALTER TABLE work_items ADD COLUMN IF NOT EXISTS severity TEXT")
        .execute(pool)
        .await?;

    // 2. The 1:1 bug-detail sidecar (kind-gated at the tool layer). UNIQUE
    //    item_id makes it a true 1:1; ON DELETE CASCADE ties its lifetime to the
    //    work item. Every descriptive column is nullable — a bug is reported
    //    first and filled in during triage.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS work_item_bug_details (
            id                 BIGSERIAL PRIMARY KEY,
            item_id            BIGINT NOT NULL UNIQUE REFERENCES work_items(id) ON DELETE CASCADE,
            reproduction_steps TEXT,
            expected_behavior  TEXT,
            actual_behavior    TEXT,
            environment        TEXT,
            affected_version   TEXT,
            fixed_in_version   TEXT,
            root_cause         TEXT,
            is_regression      BOOLEAN NOT NULL DEFAULT FALSE,
            reported_by        TEXT,
            reported_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
            triaged_by         TEXT,
            triaged_at         TIMESTAMPTZ,
            resolution         TEXT,
            created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
            updated_at         TIMESTAMPTZ NOT NULL DEFAULT now()
        )",
    )
    .execute(pool)
    .await?;

    // resolution vocabulary CHECK, built from the closed BugResolution enum
    // (the same TEXT + CHECK + closed-enum idiom as kind/status, ADR-003).
    super::v4_work_items::install_check(
        pool,
        "work_item_bug_details",
        "work_item_bug_details_resolution_check",
        &format!(
            "resolution IS NULL OR resolution IN ({})",
            crate::tracker::severity::resolution_sql_in_list()
        ),
    )
    .await?;

    // 3. Indexes. Severity-ordered bug queues; the untriaged-bug queue. Both are
    //    partial (`WHERE …`) so non-bug rows pay nothing.
    for idx in [
        "CREATE INDEX IF NOT EXISTS idx_work_items_severity \
            ON work_items(severity, priority DESC) WHERE severity IS NOT NULL",
        "CREATE INDEX IF NOT EXISTS idx_wi_bug_details_untriaged \
            ON work_item_bug_details(item_id) WHERE triaged_at IS NULL",
    ] {
        sqlx::query(idx).execute(pool).await?;
    }

    // 4. The `idx_work_items_active` partial index (from v4) enumerates the
    //    active statuses in its predicate; a partial-index predicate cannot be
    //    ALTERed, so drop+recreate it widened to include triage/confirmed (so
    //    active-bug lookups stay indexed).
    sqlx::query("DROP INDEX IF EXISTS idx_work_items_active")
        .execute(pool)
        .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_work_items_active ON work_items(status) \
            WHERE status IN ('pending','triage','confirmed','ready','in_progress','blocked')",
    )
    .execute(pool)
    .await?;

    // 5. Deterministically rebuild the kind/status/severity CHECKs from the
    //    closed Rust enums now that `bug`/`triage`/`confirmed` are in the
    //    vocabularies and the `severity` column exists (the column-existence
    //    guard inside this reconcile is now satisfied). The every-boot reconcile
    //    in `migrate()` also covers this; doing it here makes the new vocabulary
    //    usable within this same migration step.
    super::v4_work_items::install_work_items_checks(pool).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        // Pinning the constant — changing it is a schema-breaking event.
        assert_eq!(BUG_TRACKER_V1, 12);
        assert_eq!(BUG_TRACKER_V1_NAME, "bug_tracker_v1");
    }
}
