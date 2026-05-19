//! Memory-server Phase 11.4: latent-pipeline quality validator cron.
//!
//! When the operator opts into the latent pipeline
//! (`[memory.latent_pipeline] backend = "qwen3-rlv1"`), this cron
//! periodically A/Bs the text-mediated and latent paths on a fixed
//! sample of recent prompts and records `(text_score, latent_score,
//! delta)` per sample in `pgmcp_metadata`.
//!
//! If the rolling-window mean of `text_score − latent_score` exceeds
//! `quality_regression_threshold`, the dispatcher demotes the active
//! pipeline back to text (handled by the daemon's startup probe +
//! periodic `pgmcp_metadata.latent_pipeline_active` flag check).
//!
//! Default `[memory.latent_pipeline] backend = "disabled"` makes this
//! cron a no-op for stock installs.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use sqlx::PgPool;
use tracing::{info, warn};

use crate::stats::tracker::StatsTracker;

/// Persist the rolling quality summary into `pgmcp_metadata` so
/// operators can introspect the auto-downgrade decision.
pub async fn record_quality_window(
    pool: &PgPool,
    window_days: i64,
    threshold: f32,
) -> Result<QualitySummary, sqlx::Error> {
    let row: Option<(Option<f64>, Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT AVG(delta), COUNT(*),
                COUNT(*) FILTER (WHERE delta > 0)
           FROM (
             SELECT (value::json ->> 'delta')::float8 AS delta
               FROM pgmcp_metadata
              WHERE key LIKE 'latent_quality_sample:%'
                AND (value::json ->> 'recorded_at')::timestamptz
                    > NOW() - ($1::int * interval '1 day')
           ) s",
    )
    .bind(window_days as i32)
    .fetch_optional(pool)
    .await?;
    let (avg, total, regressions) = match row {
        Some((a, t, r)) => (a.unwrap_or(0.0), t.unwrap_or(0), r.unwrap_or(0)),
        None => (0.0, 0, 0),
    };
    let summary = QualitySummary {
        window_days,
        avg_delta: avg as f32,
        sample_count: total,
        regression_count: regressions,
        threshold,
        auto_downgrade_recommended: (avg as f32) > threshold,
    };
    let body = serde_json::json!(summary);
    sqlx::query(
        "INSERT INTO pgmcp_metadata (key, value)
         VALUES ('latent_pipeline_quality_summary', $1)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(body.to_string())
    .execute(pool)
    .await?;
    if summary.auto_downgrade_recommended {
        sqlx::query(
            "INSERT INTO pgmcp_metadata (key, value) VALUES ('latent_pipeline_active', 'false')
             ON CONFLICT (key) DO UPDATE SET value = 'false'",
        )
        .execute(pool)
        .await?;
    }
    Ok(summary)
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct QualitySummary {
    pub window_days: i64,
    pub avg_delta: f32,
    pub sample_count: i64,
    pub regression_count: i64,
    pub threshold: f32,
    pub auto_downgrade_recommended: bool,
}

pub async fn run_or_log(
    pool: Arc<PgPool>,
    stats: Arc<StatsTracker>,
    window_days: i64,
    threshold: f32,
) {
    let _ = stats.cron_executions.fetch_add(1, Ordering::Relaxed);
    match record_quality_window(&pool, window_days, threshold).await {
        Ok(summary) => {
            stats
                .memory_latent_quality_samples
                .store(summary.sample_count as u64, Ordering::Relaxed);
            stats
                .memory_latent_quality_regressions
                .store(summary.regression_count as u64, Ordering::Relaxed);
            if summary.auto_downgrade_recommended {
                stats
                    .memory_latent_pipeline_fallbacks
                    .fetch_add(1, Ordering::Relaxed);
                warn!(
                    avg_delta = summary.avg_delta,
                    threshold,
                    sample_count = summary.sample_count,
                    "latent_pipeline_quality cron: quality regression detected — recommending auto-downgrade"
                );
            } else {
                info!(
                    avg_delta = summary.avg_delta,
                    sample_count = summary.sample_count,
                    "latent_pipeline_quality cron: window clean"
                );
            }
        }
        Err(e) => {
            stats.cron_panics.fetch_add(1, Ordering::Relaxed);
            warn!(error = %e, "latent_pipeline_quality cron: aggregation failed");
        }
    }
}
