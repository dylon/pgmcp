//! Migration step 44: `data_table_links` bridge + bug reproduction-criterion
//! columns on `work_item_bug_details`.
//!
//! Two associations that had no representation before (ADR-023):
//!   - `data_table_links` — a generic m:n bridge tying a data table to the
//!     experiment or work-item it backs (e.g. a benchmark/measurement table that
//!     records an experiment's samples). Mirrors the `work_item_experiment`
//!     bridge; data tables previously associated only with a `project_id`.
//!   - `work_item_bug_details.{verification_command, expected_signal,
//!     criterion_locked_at}` — a *machine-checkable* reproduction criterion an
//!     agent registers so that, when it asserts the bug fixed
//!     (`work_item_assert_fixed`), the claim is *checked* (CI/experiment evidence
//!     against the frozen criterion) rather than trusted. `criterion_locked_at`
//!     is the anti-tamper freeze, mirroring an experiment hypothesis's
//!     `criterion_locked_at`.
//!
//! Additive, idempotent, version-gated.

use sqlx::PgPool;

use crate::datatable::link_target::LinkTargetType;

pub(super) const DATA_TABLE_LINKS_AND_BUG_CRITERIA: i32 = 44;
pub(super) const DATA_TABLE_LINKS_AND_BUG_CRITERIA_NAME: &str = "data_table_links_and_bug_criteria";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    // --- data_table_links --------------------------------------------------
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS data_table_links (
            id          BIGSERIAL PRIMARY KEY,
            table_id    BIGINT      NOT NULL REFERENCES data_tables(id) ON DELETE CASCADE,
            target_type TEXT        NOT NULL,
            target_id   BIGINT      NOT NULL,
            role        TEXT,
            created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
            UNIQUE (table_id, target_type, target_id)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS ix_data_table_links_target \
            ON data_table_links (target_type, target_id)",
    )
    .execute(pool)
    .await?;

    super::v4_work_items::install_check(
        pool,
        "data_table_links",
        "data_table_links_target_type_check",
        &format!("target_type IN ({})", LinkTargetType::sql_in_list()),
    )
    .await?;

    // --- bug reproduction-criterion columns --------------------------------
    for col in [
        "ALTER TABLE work_item_bug_details ADD COLUMN IF NOT EXISTS verification_command TEXT",
        "ALTER TABLE work_item_bug_details ADD COLUMN IF NOT EXISTS expected_signal TEXT",
        "ALTER TABLE work_item_bug_details ADD COLUMN IF NOT EXISTS criterion_locked_at TIMESTAMPTZ",
    ] {
        sqlx::query(col).execute(pool).await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        assert_eq!(DATA_TABLE_LINKS_AND_BUG_CRITERIA, 44);
        assert_eq!(
            DATA_TABLE_LINKS_AND_BUG_CRITERIA_NAME,
            "data_table_links_and_bug_criteria"
        );
    }
}
