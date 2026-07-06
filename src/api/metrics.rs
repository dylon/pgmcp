//! `GET /api/metrics` — time-series feed for the webui's Grafana-style Metrics
//! dashboard.
//!
//! ## Endpoint contract (the frontend normalizer depends on this exact shape)
//!
//! Request: `?series=<tool_calls|cron|quality>&bucket=<hour|day>&since_minutes=<int>`
//!
//! Response:
//! ```json
//! {"series":"tool_calls","bucket":"hour","since_minutes":1440,
//!  "buckets":[{"ts":"2026-07-05T14:00:00Z","calls":123,"errors":4,"avg_ms":12.5}],
//!  "server_seq":0}
//! ```
//!
//! Per-series `buckets[]` shapes (all `ts` are chrono `DateTime<Utc>`,
//! serialized as RFC-3339 with a `Z` suffix — the same encoding every other
//! pgmcp JSON endpoint emits for timestamps):
//!
//! * `tool_calls` → `{ts, calls, errors, avg_ms}`
//! * `cron`       → `{ts, runs, failures}`
//! * `quality`    → `{ts, overall_gpa, engineering_gpa, architecture_gpa, security_gpa}`
//!
//! ## Behavior
//!
//! Read-only: it only issues the bucketed `SELECT`s in
//! [`crate::db::queries::dashboard_metrics`]. Defaults are
//! `series=tool_calls`, `bucket=hour`, `since_minutes=1440` (clamped to
//! `1..=44640`, i.e. 1 minute .. 31 days). Every non-happy path degrades to a
//! well-formed empty series (`buckets: []`) rather than an HTTP error:
//!
//! * a null pool (CLI mode / mock `DbClient`) → empty buckets,
//! * an unknown `series` value → empty buckets,
//! * a failed query → logged at `error!` (ADR-021: a swallowed DB error is an
//!   `error!`, never a `warn!`) then empty buckets.
//!
//! `server_seq` is a constant `0`: this feed is a plain aggregate and is not
//! reconciled against the realtime event-log sequence the way the mutating
//! endpoints are.

use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use serde_json::{Value, json};

use super::ApiState;
use crate::db::queries::dashboard_metrics;

/// Default lookback window (minutes) when `since_minutes` is absent: 24 h.
const DEFAULT_SINCE_MINUTES: i64 = 1_440;
/// Lower clamp for the lookback window: 1 minute.
const MIN_SINCE_MINUTES: i64 = 1;
/// Upper clamp for the lookback window: 44 640 minutes (31 days).
const MAX_SINCE_MINUTES: i64 = 44_640;

/// Query string for `GET /api/metrics`. Every field is optional (each carries
/// `#[serde(default)]` so an absent query key deserializes to `None` under
/// `serde_urlencoded`, matching the sibling `StatsQuery` in `handlers.rs`).
#[derive(Debug, Deserialize)]
pub struct MetricsParams {
    #[serde(default)]
    pub series: Option<String>,
    #[serde(default)]
    pub bucket: Option<String>,
    #[serde(default)]
    pub since_minutes: Option<i64>,
}

/// Normalize `series` to its trimmed, lowercased form (default `tool_calls`).
/// An unrecognized value is preserved verbatim so the response echoes what was
/// asked while the dispatch falls through to an empty series.
fn normalize_series(raw: Option<&str>) -> String {
    raw.map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("tool_calls")
        .to_ascii_lowercase()
}

/// Normalize `bucket` to an allow-listed `date_trunc` field. Only `"day"`
/// (case-insensitive) maps to `"day"`; everything else — including absent or
/// garbage input — is `"hour"`. This is the literal ultimately bound into
/// `date_trunc($1, …)`, so the allow-list is also the injection guard.
fn normalize_bucket(raw: Option<&str>) -> String {
    match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("day") => "day".to_string(),
        _ => "hour".to_string(),
    }
}

/// Build the response envelope. `buckets` is pre-serialized so every exit path
/// (null pool, unknown series, query error, success) shares one shape.
fn envelope(series: &str, bucket: &str, since_minutes: i64, buckets: Value) -> Json<Value> {
    Json(json!({
        "series": series,
        "bucket": bucket,
        "since_minutes": since_minutes,
        "buckets": buckets,
        "server_seq": 0,
    }))
}

/// Serialize a query result set to a JSON array, degrading to `[]` (with an
/// `error!`) if serialization somehow fails. Keeps the three dispatch arms terse.
fn to_buckets<T: serde::Serialize>(series: &str, rows: T) -> Value {
    serde_json::to_value(rows).unwrap_or_else(|e| {
        tracing::error!(error = %e, series = %series, "metrics: bucket serialization failed");
        json!([])
    })
}

pub async fn metrics(
    State(state): State<ApiState>,
    Query(params): Query<MetricsParams>,
) -> Json<Value> {
    let series = normalize_series(params.series.as_deref());
    let bucket = normalize_bucket(params.bucket.as_deref());
    let since_minutes = params
        .since_minutes
        .unwrap_or(DEFAULT_SINCE_MINUTES)
        .clamp(MIN_SINCE_MINUTES, MAX_SINCE_MINUTES);

    // Null pool (CLI mode / mock DbClient): still return a well-formed empty
    // series instead of a 500.
    let Some(pool) = state.db.pool() else {
        return envelope(&series, &bucket, since_minutes, json!([]));
    };

    let buckets = match series.as_str() {
        "tool_calls" => {
            match dashboard_metrics::tool_call_series(pool, &bucket, since_minutes).await {
                Ok(rows) => to_buckets(&series, rows),
                Err(e) => {
                    tracing::error!(error = %e, series = %series, bucket = %bucket, "metrics: tool_calls query failed");
                    json!([])
                }
            }
        }
        "cron" => match dashboard_metrics::cron_run_series(pool, &bucket, since_minutes).await {
            Ok(rows) => to_buckets(&series, rows),
            Err(e) => {
                tracing::error!(error = %e, series = %series, bucket = %bucket, "metrics: cron query failed");
                json!([])
            }
        },
        "quality" => {
            match dashboard_metrics::quality_gpa_series(pool, &bucket, since_minutes).await {
                Ok(rows) => to_buckets(&series, rows),
                Err(e) => {
                    tracing::error!(error = %e, series = %series, bucket = %bucket, "metrics: quality query failed");
                    json!([])
                }
            }
        }
        // Unknown series: return an empty series rather than erroring (contract).
        _ => json!([]),
    };

    envelope(&series, &bucket, since_minutes, buckets)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_bucket_allow_lists_hour_and_day() {
        assert_eq!(normalize_bucket(Some("day")), "day");
        assert_eq!(normalize_bucket(Some("DAY")), "day");
        assert_eq!(normalize_bucket(Some("  Day  ")), "day");
        assert_eq!(normalize_bucket(Some("hour")), "hour");
        // Anything not exactly "day" collapses to the safe default.
        assert_eq!(normalize_bucket(Some("week")), "hour");
        assert_eq!(normalize_bucket(Some("")), "hour");
        assert_eq!(normalize_bucket(None), "hour");
    }

    #[test]
    fn normalize_series_trims_lowercases_and_defaults() {
        assert_eq!(normalize_series(None), "tool_calls");
        assert_eq!(normalize_series(Some("")), "tool_calls");
        assert_eq!(normalize_series(Some("   ")), "tool_calls");
        assert_eq!(normalize_series(Some("  Cron ")), "cron");
        assert_eq!(normalize_series(Some("QUALITY")), "quality");
        // Unknown values are preserved (dispatch falls through to empty).
        assert_eq!(normalize_series(Some("bogus")), "bogus");
    }

    #[test]
    fn envelope_has_stable_contract_shape() {
        let Json(v) = envelope("tool_calls", "hour", 1_440, json!([]));
        assert_eq!(v["series"], "tool_calls");
        assert_eq!(v["bucket"], "hour");
        assert_eq!(v["since_minutes"], 1_440);
        assert!(v["buckets"].is_array());
        assert_eq!(v["buckets"].as_array().map(Vec::len), Some(0));
        assert_eq!(v["server_seq"], 0);
    }

    #[test]
    fn to_buckets_serializes_rows_to_a_json_array() {
        let rows = vec![dashboard_metrics::CronBucket {
            ts: chrono::Utc::now(),
            runs: 7,
            failures: 2,
        }];
        let v = to_buckets("cron", rows);
        assert!(v.is_array());
        assert_eq!(v[0]["runs"], 7);
        assert_eq!(v[0]["failures"], 2);
        assert!(v[0]["ts"].is_string());
    }
}
