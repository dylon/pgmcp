//! Migration step 40: `cron_run_history` — the durable cron-run ledger.
//!
//! The in-process scheduler (`src/cron/scheduler.rs`) is 100% in-memory: a
//! restart loses every timer and `record_cron_outcome` keeps only the latest
//! outcome per job in a `DashMap`. This append-only ledger persists every run
//! (scheduled / manual / startup) with its intrinsics (duration, RSS delta,
//! thread delta, job counters) and any failure, so that (a) startup can compute
//! each job's next-due time from its last persisted success (restart-survival)
//! and (b) the `cron_history` MCP tool can query it. See ADR-018 and the writer
//! at `src/cron/history/`.
//!
//! The three TEXT vocabularies (`trigger_source` / `outcome` / `skip_reason`)
//! are closed-enum CHECKs installed via the stamp-aware `install_check` so a
//! later enum change re-applies via DROP+ADD (ADR-003 idiom). The partial index
//! `WHERE outcome = 'ok'` serves the restart hot path. Additive, idempotent,
//! version-gated.

use sqlx::PgPool;

use crate::cron::history::vocab;

pub(super) const CRON_RUN_HISTORY: i32 = 40;
pub(super) const CRON_RUN_HISTORY_NAME: &str = "cron_run_history";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS cron_run_history (
            id              BIGSERIAL PRIMARY KEY,
            job_name        TEXT        NOT NULL,
            trigger_source  TEXT        NOT NULL DEFAULT 'scheduled',
            outcome         TEXT        NOT NULL,
            skip_reason     TEXT,
            error_detail    TEXT,
            project         TEXT,
            started_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
            completed_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
            duration_ms     BIGINT      NOT NULL DEFAULT 0,
            rss_mb_start    BIGINT,
            rss_mb_end      BIGINT,
            rss_mb_delta    BIGINT,
            threads_start   INTEGER,
            threads_end     INTEGER,
            threads_delta   INTEGER,
            counters        JSONB       NOT NULL DEFAULT '{}'::jsonb
        )",
    )
    .execute(pool)
    .await?;

    for idx in [
        // Restart-survival hot path: last successful completion per job.
        "CREATE INDEX IF NOT EXISTS ix_cron_history_job_ok_completed \
            ON cron_run_history (job_name, completed_at DESC) WHERE outcome = 'ok'",
        // `cron_history` tool: recent runs per job.
        "CREATE INDEX IF NOT EXISTS ix_cron_history_job_time \
            ON cron_run_history (job_name, completed_at DESC)",
        // Retention sweep (`db-maintenance`).
        "CREATE INDEX IF NOT EXISTS ix_cron_history_completed \
            ON cron_run_history (completed_at)",
    ] {
        sqlx::query(idx).execute(pool).await?;
    }

    super::v4_work_items::install_check(
        pool,
        "cron_run_history",
        "cron_run_history_trigger_source_check",
        &format!(
            "trigger_source IN ({})",
            vocab::trigger_source_sql_in_list()
        ),
    )
    .await?;
    super::v4_work_items::install_check(
        pool,
        "cron_run_history",
        "cron_run_history_outcome_check",
        &format!("outcome IN ({})", vocab::outcome_sql_in_list()),
    )
    .await?;
    super::v4_work_items::install_check(
        pool,
        "cron_run_history",
        "cron_run_history_skip_reason_check",
        &format!(
            "skip_reason IS NULL OR skip_reason IN ({})",
            vocab::skip_reason_sql_in_list()
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
        assert_eq!(CRON_RUN_HISTORY, 40);
        assert_eq!(CRON_RUN_HISTORY_NAME, "cron_run_history");
    }
}
