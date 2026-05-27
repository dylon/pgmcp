//! `trajectory_similarity` — MSM trajectory retrieval + success/fail trend
//! (Part B4).
//!
//! Retrieves the most similar past RLM runs to a probe by Move-Split-Merge
//! distance over their step sequences, and classifies whether the probe
//! trends toward success or failure (k-NN over the labeled cohorts). The
//! split/merge cost `c` is the persisted adaptive value (online-tuned by
//! `AdaptiveMsm`); pass `recalibrate_c` to re-tune + persist.

#![allow(unused_imports)]

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::context::SystemContext;
use crate::fuzzy::trajectory_index::{
    DEFAULT_MSM_C, TrajectoryIndex, calibrate_adaptive_c, classify_trend, load_msm_c, store_msm_c,
};
use crate::mcp::server::TrajectorySimilarityParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

pub async fn tool_trajectory_similarity(
    ctx: &SystemContext,
    params: TrajectorySimilarityParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "trajectory_similarity", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let k = params.k.unwrap_or(5).clamp(1, 50);

    // Resolve the probe series + the row id to exclude (self-match).
    let (probe, exclude_id): (Vec<f64>, Option<i64>) = if let Some(series) = &params.probe_series {
        (series.clone(), None)
    } else if let Some(tid) = &params.task_id {
        let task_uuid = Uuid::parse_str(tid)
            .map_err(|e| McpError::invalid_params(format!("bad task_id: {e}"), None))?;
        let row: Option<(i64, Vec<f64>)> = sqlx::query_as(
            "SELECT id, encoded_series FROM agent_trajectories
             WHERE task_id = $1 ORDER BY id DESC LIMIT 1",
        )
        .bind(task_uuid)
        .fetch_optional(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("probe lookup: {e}"), None))?;
        match row {
            Some((id, series)) => (series, Some(id)),
            None => return Err(McpError::invalid_params("no trajectory for task_id", None)),
        }
    } else {
        return Err(McpError::invalid_params(
            "task_id or probe_series required",
            None,
        ));
    };
    if probe.is_empty() {
        return Err(McpError::invalid_params("probe series is empty", None));
    }

    // Load all trajectories for the index.
    let all_rows: Vec<(i64, Vec<f64>)> = sqlx::query_as(
        "SELECT id, encoded_series FROM agent_trajectories WHERE cardinality(encoded_series) > 0",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("load trajectories: {e}"), None))?;

    // Keep success labels fresh from explicit outcomes (Part-A↔B seam) BEFORE
    // reading cohorts — so both the trend and any recalibration see the
    // latest labels.
    let _ = crate::fuzzy::trajectory_index::label_trajectories_from_outcomes(pool).await;
    let success_rows = load_cohort(pool, true).await.unwrap_or_default();
    let fail_rows = load_cohort(pool, false).await.unwrap_or_default();

    let mut c = load_msm_c(pool).await.unwrap_or(DEFAULT_MSM_C);
    if params.recalibrate_c.unwrap_or(false) {
        // Cohort-separation calibration with the LOO precision guard.
        c = calibrate_adaptive_c(&success_rows, &fail_rows, c, 64);
        let _ = store_msm_c(pool, c).await;
    }

    let idx = TrajectoryIndex::new(all_rows, c);
    let indexed = idx.len();
    let nearest = idx.nearest(&probe, k, exclude_id);

    // Success/fail cohort trend (reuses the cohorts loaded above).
    let trend = classify_trend(&probe, success_rows, fail_rows, k, c);

    json_result(&json!({
        "k": k,
        "msm_c": c,
        "indexed": indexed,
        "nearest": nearest
            .iter()
            .map(|(id, d)| json!({"trajectory_id": id, "distance": d}))
            .collect::<Vec<_>>(),
        "trend": trend.map(|(succ, sm, fm)| {
            json!({
                "predicted_success": succ,
                "success_mean_distance": sm,
                "fail_mean_distance": fm,
            })
        }),
    }))
}

/// Stage 5d: online recognition of a partial / in-progress trajectory against
/// the live record cohorts (work_item progress, file churn) via MSM. Feed the
/// unfolding series; get the nearest known trajectories for early-warning.
pub async fn tool_recognize_trajectory(
    ctx: &SystemContext,
    params: crate::mcp::server::RecognizeTrajectoryParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "recognize_trajectory", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    if params.series.is_empty() {
        return Err(McpError::invalid_params("series must be non-empty", None));
    }
    let k = params.k.unwrap_or(5).clamp(1, 50) as usize;
    let msm_c = params.msm_c.unwrap_or(0.1);
    let nearest = crate::cron::trajectory_similarity::recognize_partial_trajectory(
        pool,
        &params.node_type,
        &params.series,
        k,
        msm_c,
    )
    .await
    .map_err(|e| McpError::internal_error(format!("recognize failed: {e}"), None))?;
    json_result(&json!({
        "node_type": params.node_type,
        "partial_len": params.series.len(),
        "nearest": nearest
            .iter()
            .map(|(id, d)| json!({"node_id": id, "msm_distance": d}))
            .collect::<Vec<_>>(),
    }))
}

async fn load_cohort(pool: &PgPool, success: bool) -> Result<Vec<(i64, Vec<f64>)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, encoded_series FROM agent_trajectories
         WHERE success = $1 AND cardinality(encoded_series) > 0",
    )
    .bind(success)
    .fetch_all(pool)
    .await
}
