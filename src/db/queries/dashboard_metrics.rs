//! Read-only time-series aggregation queries powering the webui's
//! Grafana-style **Metrics** dashboard (`GET /api/metrics`, handler in
//! `src/api/metrics.rs`).
//!
//! Three independent series, each bucketed by `date_trunc(<hour|day>, …)` over
//! an existing durable table — nothing here writes:
//!
//! | series       | table                    | ts column      | metrics                          |
//! | ------------ | ------------------------ | -------------- | -------------------------------- |
//! | `tool_calls` | `mcp_tool_calls`         | `ts`           | calls, errors, mean latency (ms) |
//! | `cron`       | `cron_run_history`       | `started_at`   | runs, failures                   |
//! | `quality`    | `quality_report_history` | `computed_at`  | portfolio-mean GPA pillars       |
//!
//! ## Parameter binding (ADR: never string-interpolate values)
//!
//! The `bucket` granularity is an allow-listed `date_trunc` field literal
//! (`"hour"` / `"day"`, normalized by the caller in `src/api/metrics.rs`) bound
//! as `$1`; the lookback window is a caller-clamped minute count bound as `$2`
//! and fed to `make_interval(mins => $2)` (mirroring the `mcp_tool_telemetry`
//! rollup in `src/api/handlers.rs`). Values are never interpolated into the SQL
//! string.
//!
//! ## Namespace note
//!
//! The flattened `crate::db::queries::metrics` namespace is already owned by the
//! design / complexity-metric queries (`src/db/queries/metrics.rs`), so this
//! dashboard surface keeps its **own** namespace
//! (`crate::db::queries::dashboard_metrics::*`) — declared as a named `pub mod`
//! in the `src/db/queries.rs` facade, mirroring the `digest` module rather than
//! being flattened with `pub use`.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;

/// One `tool_calls` bucket. `errors` counts any non-`ok` outcome
/// (`error` / `timeout` / `cancelled`), matching the `mcp_tool_telemetry`
/// endpoint's `error_count` semantics. `avg_ms` is the mean `duration_ms` over
/// the bucket; it is `Option` only to mirror sqlx's nullable decode of
/// `AVG(...)` — a non-empty bucket always yields a value.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ToolCallBucket {
    pub ts: DateTime<Utc>,
    pub calls: i64,
    pub errors: i64,
    pub avg_ms: Option<f64>,
}

/// One `cron` bucket. `runs` counts every recorded run in the bucket (including
/// `skipped` ticks); `failures` counts `failed` + `panicked` outcomes, matching
/// the `cron_job_rollup` fail-count definition.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct CronBucket {
    pub ts: DateTime<Utc>,
    pub runs: i64,
    pub failures: i64,
}

/// One `quality` bucket: portfolio-mean GPA pillars (`AVG` across every project
/// snapshotted into `quality_report_history` in the bucket). Each pillar is
/// nullable — a pillar reports N/A when all its dimensions are data-absent, so
/// the stored GPA (and therefore its `AVG`) can be NULL.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct QualityBucket {
    pub ts: DateTime<Utc>,
    pub overall_gpa: Option<f64>,
    pub engineering_gpa: Option<f64>,
    pub architecture_gpa: Option<f64>,
    pub security_gpa: Option<f64>,
}

/// `tool_calls` series over `mcp_tool_calls`, oldest bucket first.
///
/// `bucket` must be an allow-listed `date_trunc` field (`"hour"` / `"day"`);
/// `since_minutes` is the lookback window in minutes, caller-clamped to a sane
/// range (`src/api/metrics.rs` clamps to `1..=44640`) and cast to `int` for
/// `make_interval`.
pub async fn tool_call_series(
    pool: &PgPool,
    bucket: &str,
    since_minutes: i64,
) -> Result<Vec<ToolCallBucket>, sqlx::Error> {
    sqlx::query_as::<_, ToolCallBucket>(
        "SELECT date_trunc($1, ts)                              AS ts,
                COUNT(*)::BIGINT                                AS calls,
                COUNT(*) FILTER (WHERE outcome <> 'ok')::BIGINT AS errors,
                AVG(duration_ms)::float8                        AS avg_ms
           FROM mcp_tool_calls
          WHERE ts > now() - make_interval(mins => $2)
          GROUP BY 1
          ORDER BY 1",
    )
    .bind(bucket)
    .bind(since_minutes as i32)
    .fetch_all(pool)
    .await
}

/// `cron` series over `cron_run_history`, oldest bucket first. See
/// [`tool_call_series`] for the `bucket` / `since_minutes` contract.
pub async fn cron_run_series(
    pool: &PgPool,
    bucket: &str,
    since_minutes: i64,
) -> Result<Vec<CronBucket>, sqlx::Error> {
    sqlx::query_as::<_, CronBucket>(
        "SELECT date_trunc($1, started_at)                                       AS ts,
                COUNT(*)::BIGINT                                                 AS runs,
                COUNT(*) FILTER (WHERE outcome IN ('failed','panicked'))::BIGINT AS failures
           FROM cron_run_history
          WHERE started_at > now() - make_interval(mins => $2)
          GROUP BY 1
          ORDER BY 1",
    )
    .bind(bucket)
    .bind(since_minutes as i32)
    .fetch_all(pool)
    .await
}

/// `quality` series over `quality_report_history`, oldest bucket first. See
/// [`tool_call_series`] for the `bucket` / `since_minutes` contract.
pub async fn quality_gpa_series(
    pool: &PgPool,
    bucket: &str,
    since_minutes: i64,
) -> Result<Vec<QualityBucket>, sqlx::Error> {
    sqlx::query_as::<_, QualityBucket>(
        "SELECT date_trunc($1, computed_at)   AS ts,
                AVG(overall_gpa)::float8      AS overall_gpa,
                AVG(engineering_gpa)::float8  AS engineering_gpa,
                AVG(architecture_gpa)::float8 AS architecture_gpa,
                AVG(security_gpa)::float8     AS security_gpa
           FROM quality_report_history
          WHERE computed_at > now() - make_interval(mins => $2)
          GROUP BY 1
          ORDER BY 1",
    )
    .bind(bucket)
    .bind(since_minutes as i32)
    .fetch_all(pool)
    .await
}
