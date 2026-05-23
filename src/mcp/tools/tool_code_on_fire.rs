//! `tool_code_on_fire` — Tornhill hotspot intersection (SOTA Phase 1, A2).
//!
//! Per-function intersection of churn × complexity from *Your Code as a Crime
//! Scene*. A function is "on fire" iff its file is in the top quartile of
//! `file_metrics.churn_rate` AND its cyclomatic complexity is in the top
//! quartile OR its maintainability_index is in the bottom quartile.

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

/// One result row returned by the SQL query below.
#[derive(Debug, sqlx::FromRow)]
struct CodeOnFireRow {
    relative_path: String,
    language: String,
    churn_rate: f64,
    commit_count: i32,
    function_name: String,
    start_line: i32,
    end_line: i32,
    cyclomatic: i32,
    cognitive: i32,
    maintainability_index: f64,
    npath: i64,
}

pub async fn tool_code_on_fire(
    ctx: &SystemContext,
    params: CodeOnFireParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .code_on_fire_scans
        .fetch_add(1, Ordering::Relaxed);

    let limit = params.limit.unwrap_or(30);
    let churn_q = params.churn_quartile.unwrap_or(0.75);
    let complexity_q = params.complexity_quartile.unwrap_or(0.75);
    let mi_q = 1.0 - complexity_q; // mirror quartile for MI (bottom)
    let mode = params.mode.as_deref().unwrap_or("intersect");

    debug!(
        tool = "code_on_fire",
        project = %params.project,
        limit,
        churn_q,
        complexity_q,
        mode,
        "MCP tool invoked",
    );

    let pool = ctx
        .db()
        .pool()
        .expect("code_on_fire needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>");

    // Build the WHERE clause based on mode. The CTE pipeline computes
    // quartile thresholds inline so the result is per-project even when
    // global percentiles vary widely across projects.
    let where_clause = match mode {
        "union" => {
            "(pf.churn_rate >= churn_q.p OR fn.cyclomatic >= cyclo_q.p OR fn.maintainability_index <= mi_q.p)"
        }
        "max" => "TRUE", // rank by composite, no filter
        _ => {
            "pf.churn_rate >= churn_q.p AND (fn.cyclomatic >= cyclo_q.p OR fn.maintainability_index <= mi_q.p)"
        }
    };

    let sql = format!(
        "WITH project_files AS (
            SELECT f.id AS file_id, f.relative_path, f.language,
                   COALESCE(fm.churn_rate, 0.0) AS churn_rate,
                   COALESCE(fm.commit_count, 0) AS commit_count
            FROM indexed_files f
            LEFT JOIN file_metrics fm ON fm.file_id = f.id
            JOIN projects p ON f.project_id = p.id
            WHERE p.name = $1
        ),
        churn_q AS (
            SELECT COALESCE(
                PERCENTILE_CONT($3) WITHIN GROUP (ORDER BY churn_rate) FILTER (WHERE churn_rate > 0),
                0.0
            ) AS p
            FROM project_files
        ),
        fn_metrics AS (
            SELECT fm.function_id, fm.file_id,
                   fm.cyclomatic, fm.cognitive,
                   fm.maintainability_index, fm.npath,
                   fs.name, fs.start_line, fs.end_line
            FROM function_metrics fm
            JOIN file_symbols fs ON fm.function_id = fs.id
            JOIN project_files pf ON fm.file_id = pf.file_id
        ),
        cyclo_q AS (
            SELECT COALESCE(PERCENTILE_CONT($4) WITHIN GROUP (ORDER BY cyclomatic), 0) AS p
            FROM fn_metrics
        ),
        mi_q AS (
            SELECT COALESCE(PERCENTILE_CONT($5) WITHIN GROUP (ORDER BY maintainability_index), 100.0) AS p
            FROM fn_metrics
        )
        SELECT pf.relative_path,
               pf.language,
               pf.churn_rate,
               pf.commit_count,
               fn.name AS function_name,
               fn.start_line,
               fn.end_line,
               fn.cyclomatic,
               fn.cognitive,
               fn.maintainability_index,
               fn.npath
        FROM fn_metrics fn
        JOIN project_files pf ON fn.file_id = pf.file_id
        CROSS JOIN churn_q CROSS JOIN cyclo_q CROSS JOIN mi_q
        WHERE {where_clause}
        ORDER BY (
            pf.churn_rate
            * GREATEST(fn.cyclomatic, 1)
            / GREATEST(NULLIF(fn.maintainability_index, 0), 1.0)
        ) DESC NULLS LAST
        LIMIT $2"
    );

    let rows: Vec<CodeOnFireRow> = sqlx::query_as::<_, CodeOnFireRow>(&sql)
        .bind(&params.project)
        .bind(limit)
        .bind(churn_q)
        .bind(complexity_q)
        .bind(mi_q)
        .fetch_all(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("code_on_fire query failed: {}", e), None))?;

    if rows.is_empty() {
        return Ok(CallToolResult::success(vec![Content::text(
            "No 'on fire' functions found. Either the project has no churn × complexity \
intersection, or the `function-metrics` cron has not run yet (no function_metrics rows). \
Try `index_stats` to verify both file_metrics (graph-analysis) and function_metrics \
(function-metrics) have been populated.",
        )]));
    }

    let results: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let score =
                r.churn_rate * (r.cyclomatic as f64).max(1.0) / r.maintainability_index.max(1.0);
            json!({
                "file": r.relative_path,
                "language": r.language,
                "function": r.function_name,
                "start_line": r.start_line,
                "end_line": r.end_line,
                "churn_rate": r.churn_rate,
                "commit_count": r.commit_count,
                "cyclomatic": r.cyclomatic,
                "cognitive": r.cognitive,
                "maintainability_index": r.maintainability_index,
                "npath": r.npath,
                "score": score,
            })
        })
        .collect();

    // Shadow-ASR channel (Phase D2b): per-effect symbol-count breakdown.
    let effect_breakdown: Vec<serde_json::Value> = (async {
        let Some(pool) = ctx.db().pool() else {
            return Vec::new();
        };
        let project_id_opt: Option<i32> =
            sqlx::query_scalar("SELECT id FROM projects WHERE name = $1")
                .bind(&params.project)
                .fetch_optional(pool)
                .await
                .unwrap_or(None);
        match project_id_opt {
            Some(pid) => crate::mcp::tools::sema_helpers::effects::effect_counts(pool, pid)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
                .collect(),
            None => Vec::new(),
        }
    })
    .await;

    let summary = json!({
        "effect_breakdown": effect_breakdown,
        "project": params.project,
        "mode": mode,
        "churn_quartile": churn_q,
        "complexity_quartile": complexity_q,
        "returned": results.len(),
        "elapsed_ms": start.elapsed().as_millis() as u64,
        "results": results,
    });

    Ok(CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&summary)
            .unwrap_or_else(|_| "Failed to serialize results".into()),
    )]))
}
