//! `tool_anomaly_detection` — MCP tool body, extracted from `super::super::server`.

#![allow(unused_imports)]

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, LoggingLevel};
use serde_json::json;
use tracing::{debug, error, info, warn};

use crate::context::SystemContext;
use crate::mcp::server::*;

pub async fn tool_anomaly_detection(
    ctx: &SystemContext,
    params: AnomalyDetectionParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().anomaly_scans.fetch_add(1, Ordering::Relaxed);

    let limit = params.limit.unwrap_or(20);
    let contamination = params.contamination.unwrap_or(0.05);

    debug!(
        tool = "anomaly_detection",
        project = %params.project,
        limit,
        contamination,
        "MCP tool invoked",
    );

    // Get project centroid (average embedding) and per-file distances
    let project_id: Option<i32> =
        sqlx::query_scalar("SELECT id FROM projects WHERE name = $1")
            .bind(&params.project)
            .fetch_optional(ctx.db().pool().expect(
                "inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>",
            ))
            .await
            .map_err(|e| McpError::internal_error(format!("Project lookup failed: {}", e), None))?;

    let project_id = project_id.ok_or_else(|| {
        McpError::internal_error(format!("Project not found: {}", params.project), None)
    })?;

    // Phase 5 C7: resolve active embedding signature so the centroid
    // aggregate runs against the right column with the right dim cast.
    // Read-side dispatch via the closed-set helper; column + dim come
    // from the EmbeddingSignature enum (no user input → safe to
    // `format!` into the SQL). Plan reference:
    // ~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md
    // Phase 5 C7.
    let active = crate::embed::signature::read_active_signature(
        ctx.db()
            .pool()
            .expect("inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>"),
    )
    .await
    .map_err(|e| McpError::internal_error(format!("active embedding signature: {}", e), None))?;
    let col = active.read_column();
    let dim = active.dim();

    // Compute per-file average embedding distance from project centroid
    // Using SQL: avg cosine distance from average embedding
    #[derive(sqlx::FromRow)]
    struct AnomalyRow {
        file_id: i64,
        relative_path: String,
        language: String,
        line_count: i32,
        avg_distance: f64,
    }

    let sql = format!(
        "WITH project_centroid AS (
            SELECT AVG(fc.{col})::vector({dim}) as centroid
            FROM file_chunks fc
            JOIN indexed_files f ON fc.file_id = f.id
            WHERE f.project_id = $1 AND fc.{col} IS NOT NULL
        ),
        file_distances AS (
            SELECT
                f.id as file_id,
                f.relative_path,
                f.language,
                f.line_count,
                AVG(fc.{col} <=> pc.centroid) as avg_distance
            FROM file_chunks fc
            JOIN indexed_files f ON fc.file_id = f.id
            CROSS JOIN project_centroid pc
            WHERE f.project_id = $1 AND fc.{col} IS NOT NULL
            GROUP BY f.id, f.relative_path, f.language, f.line_count
        )
        SELECT file_id, relative_path, language, line_count, avg_distance
        FROM file_distances
        ORDER BY avg_distance DESC"
    );
    let rows: Vec<AnomalyRow> =
        sqlx::query_as::<_, AnomalyRow>(&sql)
            .bind(project_id)
            .fetch_all(ctx.db().pool().expect(
                "inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>",
            ))
            .await
            .map_err(|e| McpError::internal_error(format!("Anomaly query failed: {}", e), None))?;

    if rows.is_empty() {
        return Ok(CallToolResult::success(vec![Content::text(
            "No embedded files found for this project.",
        )]));
    }

    // Compute z-scores for distances
    let n = rows.len() as f64;
    let mean_dist: f64 = rows.iter().map(|r| r.avg_distance).sum::<f64>() / n;
    let variance: f64 = rows
        .iter()
        .map(|r| (r.avg_distance - mean_dist).powi(2))
        .sum::<f64>()
        / n;
    let std_dev = variance.sqrt().max(0.0001);

    // Also get metric z-scores from file_metrics
    #[derive(sqlx::FromRow)]
    struct MetricRow {
        file_id: i64,
        line_count_z: Option<f64>,
        churn_z: Option<f64>,
    }

    let metric_zscores: Vec<MetricRow> = sqlx::query_as::<_, MetricRow>(
        "WITH stats AS (
            SELECT
                AVG(f.line_count)::DOUBLE PRECISION as avg_lc,
                STDDEV_POP(f.line_count)::DOUBLE PRECISION as std_lc,
                AVG(fm.churn_rate)::DOUBLE PRECISION as avg_churn,
                STDDEV_POP(fm.churn_rate)::DOUBLE PRECISION as std_churn
            FROM indexed_files f
            LEFT JOIN file_metrics fm ON fm.file_id = f.id
            WHERE f.project_id = $1
        )
        SELECT
            f.id as file_id,
            CASE WHEN s.std_lc > 0 THEN (f.line_count - s.avg_lc) / s.std_lc ELSE 0 END as line_count_z,
            CASE WHEN s.std_churn > 0 THEN (COALESCE(fm.churn_rate, 0) - s.avg_churn) / s.std_churn ELSE 0 END as churn_z
        FROM indexed_files f
        LEFT JOIN file_metrics fm ON fm.file_id = f.id
        CROSS JOIN stats s
        WHERE f.project_id = $1"
    )
    .bind(project_id)
    .fetch_all(ctx.db().pool().expect("inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>"))
    .await
    .unwrap_or_default();

    let z_map: std::collections::HashMap<i64, (f64, f64)> = metric_zscores
        .iter()
        .map(|r| {
            (
                r.file_id,
                (r.line_count_z.unwrap_or(0.0), r.churn_z.unwrap_or(0.0)),
            )
        })
        .collect();

    // Isolation Forest (Liu-Ting-Zhou 2008): build per-file feature vectors
    // (distance / size / churn z-scores) and score by isolation ease. Replaces
    // the hand-weighted z-score sum the `contamination` param always implied.
    let features: Vec<Vec<f64>> = rows
        .iter()
        .map(|r| {
            let distance_z = (r.avg_distance - mean_dist) / std_dev;
            let (lc_z, churn_z) = z_map.get(&r.file_id).copied().unwrap_or((0.0, 0.0));
            vec![distance_z, lc_z, churn_z]
        })
        .collect();
    // ψ = 256 (paper default), 100 trees, fixed seed for reproducibility.
    let iforest = crate::code_analysis::isolation_forest::anomaly_scores(&features, 100, 256, 42);

    let mut anomalies: Vec<serde_json::Value> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let distance_z = features[i][0];
            let lc_z = features[i][1];
            let churn_z = features[i][2];
            serde_json::json!({
                "path": r.relative_path,
                "language": r.language,
                "line_count": r.line_count,
                "anomaly_score": format!("{:.4}", iforest[i]),
                "method": "isolation_forest",
                "embedding_distance": format!("{:.4}", r.avg_distance),
                "distance_zscore": format!("{:.2}", distance_z),
                "size_zscore": format!("{:.2}", lc_z),
                "churn_zscore": format!("{:.2}", churn_z),
            })
        })
        .collect();

    anomalies.sort_by(|a, b| {
        let sa: f64 = a["anomaly_score"]
            .as_str()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0.0);
        let sb: f64 = b["anomaly_score"]
            .as_str()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0.0);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });
    // `contamination` sets how many of the highest-scoring files count as
    // anomalies; `limit` caps the response. Keep the smaller of the two.
    let contamination_keep = ((contamination * rows.len() as f64).ceil() as usize).min(rows.len());
    anomalies.truncate(contamination_keep.min(limit as usize));

    // Shadow-ASR channel: per-effect symbol-count distribution. Files in
    // anomalous portions of the codebase often correlate with concentrated
    // effects (e.g. all the project's `unsafe` lives in one anomalous file).
    let effect_distribution = if let Some(pool) = ctx.db().pool() {
        crate::mcp::tools::sema_helpers::effects::effect_counts(pool, project_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    let result = serde_json::json!({
        "project": params.project,
        "contamination": contamination,
        "mean_distance": format!("{:.4}", mean_dist),
        "std_distance": format!("{:.4}", std_dev),
        "anomaly_count": anomalies.len(),
        "anomalies": anomalies,
        "effect_distribution": effect_distribution,
        "guidance": "anomaly_score is the Isolation Forest score (Liu-Ting-Zhou 2008) in (0,1]: files that a \
                     random split-tree isolates in few cuts — unusual in embedding distance, size, or churn — \
                     score near 1. `contamination` selects how many top-scoring files are returned as \
                     anomalies. They may be abandoned experiments, copied-in code, auto-generated files, or \
                     architectural outliers; the *_zscore fields explain which dimension drives the score \
                     (high distance_zscore = content unlike the project norm).",
    });

    let json = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "anomaly_detection",
        anomalies = anomalies.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
