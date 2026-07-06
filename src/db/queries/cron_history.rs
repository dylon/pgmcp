//! Read queries over the `cron_run_history` ledger (v40).
//!
//! The append-only *write* path lives in the writer module
//! (`src/cron/history/mod.rs::flush_batch`, mirroring the telemetry writer);
//! this module holds the reads consumed by the scheduler's restart-survival
//! startup (`last_successful_completions`), the `cron_history` MCP tool
//! (`recent_cron_runs` / `cron_job_rollup`), and the `db-maintenance` retention
//! sweep (`delete_cron_runs_older_than`).

use chrono::{DateTime, Utc};
use sqlx::PgPool;

/// One recent run row, as surfaced by the `cron_history` tool. The wide
/// intrinsic columns are reduced to the operator-relevant deltas.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct CronRunRow {
    pub id: i64,
    pub job_name: String,
    pub trigger_source: String,
    pub outcome: String,
    pub skip_reason: Option<String>,
    pub error_detail: Option<String>,
    pub project: Option<String>,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub duration_ms: i64,
    pub rss_mb_delta: Option<i64>,
    pub threads_delta: Option<i32>,
    pub counters: serde_json::Value,
}

/// Per-job rollup: the latest outcome, last success, and run/outcome counts.
/// `next_due` is computed by the tool (it needs the per-job interval from
/// `config.cron`, which the DB layer does not have).
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct CronJobRollupRow {
    pub job_name: String,
    pub last_outcome: String,
    pub last_completed_at: DateTime<Utc>,
    pub last_ok: Option<DateTime<Utc>>,
    pub run_count: i64,
    pub ok_count: i64,
    pub fail_count: i64,
    pub skip_count: i64,
    /// Mean run duration across the ledger for this job (NULL only if the group
    /// is somehow empty, which `GROUP BY` precludes).
    pub avg_ms: Option<i64>,
    /// The most recent non-NULL `error_detail` / `skip_reason` for this job, so
    /// the operator sees *why* a job last failed or was skipped without opening
    /// the per-run "Recent runs" table.
    pub last_error: Option<String>,
    pub last_skip_reason: Option<String>,
}

/// `(job_name, MAX(completed_at))` over successful runs — the restart-survival
/// hot path. Served by the partial index `ix_cron_history_job_ok_completed`.
pub async fn last_successful_completions(
    pool: &PgPool,
) -> Result<Vec<(String, DateTime<Utc>)>, sqlx::Error> {
    sqlx::query_as::<_, (String, DateTime<Utc>)>(
        "SELECT job_name, MAX(completed_at) AS last_ok
           FROM cron_run_history
          WHERE outcome = 'ok'
          GROUP BY job_name",
    )
    .fetch_all(pool)
    .await
}

/// Recent runs, newest first. Optionally filtered to one `job`; `limit` is
/// clamped to `1..=500` by the caller.
pub async fn recent_cron_runs(
    pool: &PgPool,
    job: Option<&str>,
    limit: i64,
) -> Result<Vec<CronRunRow>, sqlx::Error> {
    sqlx::query_as::<_, CronRunRow>(
        "SELECT id, job_name, trigger_source, outcome, skip_reason, error_detail,
                project, started_at, completed_at, duration_ms,
                rss_mb_delta, threads_delta, counters
           FROM cron_run_history
          WHERE ($1::text IS NULL OR job_name = $1)
          ORDER BY completed_at DESC
          LIMIT $2",
    )
    .bind(job)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Per-job rollup over the full ledger, ordered by `job_name`.
pub async fn cron_job_rollup(pool: &PgPool) -> Result<Vec<CronJobRollupRow>, sqlx::Error> {
    sqlx::query_as::<_, CronJobRollupRow>(
        "SELECT job_name,
                (array_agg(outcome ORDER BY completed_at DESC))[1] AS last_outcome,
                MAX(completed_at)                                  AS last_completed_at,
                MAX(completed_at) FILTER (WHERE outcome = 'ok')    AS last_ok,
                COUNT(*)                                           AS run_count,
                COUNT(*) FILTER (WHERE outcome = 'ok')             AS ok_count,
                COUNT(*) FILTER (WHERE outcome IN ('failed','panicked')) AS fail_count,
                COUNT(*) FILTER (WHERE outcome = 'skipped')        AS skip_count,
                AVG(duration_ms)::bigint                           AS avg_ms,
                (array_agg(error_detail ORDER BY completed_at DESC)
                   FILTER (WHERE error_detail IS NOT NULL))[1]     AS last_error,
                (array_agg(skip_reason ORDER BY completed_at DESC)
                   FILTER (WHERE skip_reason IS NOT NULL))[1]      AS last_skip_reason
           FROM cron_run_history
          GROUP BY job_name
          ORDER BY job_name",
    )
    .fetch_all(pool)
    .await
}

/// Delete rows older than `days` (retention sweep, run by `db-maintenance`).
/// `days <= 0` means "keep forever" → no-op. Returns the deleted row count.
pub async fn delete_cron_runs_older_than(pool: &PgPool, days: i64) -> Result<u64, sqlx::Error> {
    if days <= 0 {
        return Ok(0);
    }
    let result = sqlx::query(
        "DELETE FROM cron_run_history
          WHERE completed_at < now() - make_interval(days => $1::int)",
    )
    .bind(days as i32)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}
