//! Migration step 9: `quality_report_history_v1` — per-pillar GPA history for
//! the `quality_report` tool's trend strip. One row per run.
//!
//! GPA columns are nullable: a pillar reports `N/A` (and is excluded from the
//! overall GPA) when all its dimensions are data-absent, so there is genuinely
//! no GPA to record. `raw_summary` carries the overall grade / ORR / finding
//! count for quick history scans without recomputing.

use sqlx::PgPool;

pub(super) const QUALITY_REPORT_HISTORY_V1: i32 = 9;
pub(super) const QUALITY_REPORT_HISTORY_V1_NAME: &str = "quality_report_history_v1";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS quality_report_history (
            id BIGSERIAL PRIMARY KEY,
            project_id INTEGER REFERENCES projects(id) ON DELETE CASCADE,
            computed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
            engineering_gpa REAL,
            architecture_gpa REAL,
            security_gpa REAL,
            overall_gpa REAL,
            raw_summary JSONB NOT NULL
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS ix_qr_history_project_time
         ON quality_report_history (project_id, computed_at DESC)",
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
        assert_eq!(QUALITY_REPORT_HISTORY_V1, 9);
        assert_eq!(QUALITY_REPORT_HISTORY_V1_NAME, "quality_report_history_v1");
    }
}
