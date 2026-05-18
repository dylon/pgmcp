//! `tool_mcp_telemetry` — query the durable `mcp_tool_calls` table.
//!
//! Reports the four cuts the user typically asks about: total calls per
//! tool, latency percentiles, error rate, and breakdowns by caller agent
//! or project. The in-memory `StatsTracker::tool_telemetry` snapshot is
//! the right tool for the "right now" view (real-time, no DB round-trip)
//! and is exposed via `/api/status`. This tool is for the historical
//! lookback — the last hour, the last day — read straight from
//! `mcp_tool_calls` with SQL aggregations.

#![allow(unused_imports)]

use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::{Value, json};
use tracing::{debug, error, info};

use crate::context::SystemContext;
use crate::mcp::server::*;

pub async fn tool_mcp_tool_telemetry(
    ctx: &SystemContext,
    params: McpToolTelemetryParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    info!(
        tool = "mcp_tool_telemetry",
        aggregation = ?params.aggregation,
        since_minutes = params.since_minutes,
        "MCP tool invoked"
    );

    let pool = ctx.db().pool().ok_or_else(|| {
        McpError::internal_error("mcp_tool_telemetry requires a real PgPool", None)
    })?;

    let since_minutes = params.since_minutes.unwrap_or(60).clamp(1, 60 * 24 * 31);
    let aggregation = params
        .aggregation
        .clone()
        .unwrap_or_else(|| "summary".to_string());

    let body = match aggregation.as_str() {
        "summary" => agg_summary(pool, &params, since_minutes).await?,
        "top_tools" => agg_top_tools(pool, &params, since_minutes).await?,
        "top_callers" => agg_top_callers(pool, &params, since_minutes).await?,
        "top_projects" => agg_top_projects(pool, &params, since_minutes).await?,
        "error_rate" => agg_error_rate(pool, &params, since_minutes).await?,
        "histogram" => agg_histogram(pool, &params, since_minutes).await?,
        "raw" => agg_raw(pool, &params, since_minutes).await?,
        other => {
            return Err(McpError::invalid_params(
                format!(
                    "Unknown aggregation '{}': expected one of summary | top_tools | \
                     top_callers | top_projects | error_rate | histogram | raw",
                    other
                ),
                None,
            ));
        }
    };

    let envelope = json!({
        "aggregation": aggregation,
        "since_minutes": since_minutes,
        "filters": {
            "tool": params.tool,
            "client_name": params.client_name,
            "project": params.project,
        },
        "data": body,
    });

    debug!(
        tool = "mcp_tool_telemetry",
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed"
    );

    Ok(CallToolResult::success(vec![Content::text(
        envelope.to_string(),
    )]))
}

async fn agg_summary(
    pool: &sqlx::PgPool,
    params: &McpToolTelemetryParams,
    since_minutes: i32,
) -> Result<Value, McpError> {
    #[allow(clippy::type_complexity)]
    let rows: Vec<(String, String, Option<String>, i64, i64, f64, f64, f64, f64, i64)> =
        sqlx::query_as(
            "SELECT
                tool,
                client_name,
                project,
                COUNT(*)                                                    AS calls,
                COUNT(*) FILTER (WHERE outcome <> 'ok')                     AS errors,
                AVG(duration_ms)::float8                                    AS mean_ms,
                COALESCE(PERCENTILE_CONT(0.50) WITHIN GROUP (ORDER BY duration_ms), 0.0)::float8 AS p50_ms,
                COALESCE(PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY duration_ms), 0.0)::float8 AS p95_ms,
                COALESCE(PERCENTILE_CONT(0.99) WITHIN GROUP (ORDER BY duration_ms), 0.0)::float8 AS p99_ms,
                COALESCE(MAX(duration_ms), 0)::bigint                       AS max_ms
             FROM mcp_tool_calls
             WHERE ts > now() - ($1::int * interval '1 minute')
               AND ($2::text IS NULL OR tool = $2)
               AND ($3::text IS NULL OR client_name = $3)
               AND ($4::text IS NULL OR project = $4)
             GROUP BY tool, client_name, project
             ORDER BY calls DESC
             LIMIT 1000",
        )
        .bind(since_minutes)
        .bind(params.tool.as_deref())
        .bind(params.client_name.as_deref())
        .bind(params.project.as_deref())
        .fetch_all(pool)
        .await
        .map_err(map_db_err)?;

    let arr: Vec<Value> = rows
        .into_iter()
        .map(
            |(tool, client, project, calls, errs, mean, p50, p95, p99, max_ms)| {
                json!({
                    "tool": tool,
                    "client_name": client,
                    "project": project,
                    "calls": calls,
                    "errors": errs,
                    "mean_ms": mean,
                    "p50_ms": p50,
                    "p95_ms": p95,
                    "p99_ms": p99,
                    "max_ms": max_ms,
                })
            },
        )
        .collect();
    Ok(json!({ "rows": arr }))
}

async fn agg_top_tools(
    pool: &sqlx::PgPool,
    params: &McpToolTelemetryParams,
    since_minutes: i32,
) -> Result<Value, McpError> {
    let rows: Vec<(String, i64, i64, f64)> = sqlx::query_as(
        "SELECT tool, COUNT(*), COUNT(*) FILTER (WHERE outcome <> 'ok'),
                COALESCE(PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY duration_ms), 0.0)::float8 AS p95_ms
         FROM mcp_tool_calls
         WHERE ts > now() - ($1::int * interval '1 minute')
           AND ($2::text IS NULL OR client_name = $2)
         GROUP BY tool
         ORDER BY COUNT(*) DESC
         LIMIT 50",
    )
    .bind(since_minutes)
    .bind(params.client_name.as_deref())
    .fetch_all(pool)
    .await
    .map_err(map_db_err)?;

    Ok(json!({
        "rows": rows.into_iter().map(|(t, c, e, p)| json!({
            "tool": t, "calls": c, "errors": e, "p95_ms": p,
        })).collect::<Vec<_>>()
    }))
}

async fn agg_top_callers(
    pool: &sqlx::PgPool,
    params: &McpToolTelemetryParams,
    since_minutes: i32,
) -> Result<Value, McpError> {
    let rows: Vec<(String, i64, i64, i64)> = sqlx::query_as(
        "SELECT client_name, COUNT(*), COUNT(*) FILTER (WHERE outcome <> 'ok'),
                COUNT(DISTINCT tool)
         FROM mcp_tool_calls
         WHERE ts > now() - ($1::int * interval '1 minute')
           AND ($2::text IS NULL OR tool = $2)
         GROUP BY client_name
         ORDER BY COUNT(*) DESC
         LIMIT 50",
    )
    .bind(since_minutes)
    .bind(params.tool.as_deref())
    .fetch_all(pool)
    .await
    .map_err(map_db_err)?;

    Ok(json!({
        "rows": rows.into_iter().map(|(c, n, e, t)| json!({
            "client_name": c, "calls": n, "errors": e, "distinct_tools": t,
        })).collect::<Vec<_>>()
    }))
}

async fn agg_top_projects(
    pool: &sqlx::PgPool,
    params: &McpToolTelemetryParams,
    since_minutes: i32,
) -> Result<Value, McpError> {
    let rows: Vec<(Option<String>, i64, i64)> = sqlx::query_as(
        "SELECT project, COUNT(*), COUNT(*) FILTER (WHERE outcome <> 'ok')
         FROM mcp_tool_calls
         WHERE ts > now() - ($1::int * interval '1 minute')
           AND project IS NOT NULL
           AND ($2::text IS NULL OR tool = $2)
           AND ($3::text IS NULL OR client_name = $3)
         GROUP BY project
         ORDER BY COUNT(*) DESC
         LIMIT 50",
    )
    .bind(since_minutes)
    .bind(params.tool.as_deref())
    .bind(params.client_name.as_deref())
    .fetch_all(pool)
    .await
    .map_err(map_db_err)?;

    Ok(json!({
        "rows": rows.into_iter().map(|(p, n, e)| json!({
            "project": p, "calls": n, "errors": e,
        })).collect::<Vec<_>>()
    }))
}

async fn agg_error_rate(
    pool: &sqlx::PgPool,
    params: &McpToolTelemetryParams,
    since_minutes: i32,
) -> Result<Value, McpError> {
    let rows: Vec<(String, i64, i64, f64)> = sqlx::query_as(
        "SELECT tool, COUNT(*), COUNT(*) FILTER (WHERE outcome <> 'ok'),
                (COUNT(*) FILTER (WHERE outcome <> 'ok'))::float8 / NULLIF(COUNT(*), 0)::float8 AS error_rate
         FROM mcp_tool_calls
         WHERE ts > now() - ($1::int * interval '1 minute')
           AND ($2::text IS NULL OR client_name = $2)
         GROUP BY tool
         ORDER BY error_rate DESC NULLS LAST, COUNT(*) DESC
         LIMIT 50",
    )
    .bind(since_minutes)
    .bind(params.client_name.as_deref())
    .fetch_all(pool)
    .await
    .map_err(map_db_err)?;

    Ok(json!({
        "rows": rows.into_iter().map(|(t, c, e, r)| json!({
            "tool": t, "calls": c, "errors": e, "error_rate": r,
        })).collect::<Vec<_>>()
    }))
}

async fn agg_histogram(
    pool: &sqlx::PgPool,
    params: &McpToolTelemetryParams,
    since_minutes: i32,
) -> Result<Value, McpError> {
    // Bucket duration_ms into log-spaced bands. Bands match the PerToolStats
    // in-memory bucketing (3× spacing from 1 ms upward) so the histogram
    // shape lines up with /metrics.
    let rows: Vec<(String, i32, i64)> = sqlx::query_as(
        "WITH bucketed AS (
            SELECT
                tool,
                CASE
                    WHEN duration_ms <= 1     THEN 0
                    WHEN duration_ms <= 3     THEN 1
                    WHEN duration_ms <= 10    THEN 2
                    WHEN duration_ms <= 30    THEN 3
                    WHEN duration_ms <= 100   THEN 4
                    WHEN duration_ms <= 300   THEN 5
                    WHEN duration_ms <= 1000  THEN 6
                    WHEN duration_ms <= 3000  THEN 7
                    WHEN duration_ms <= 10000 THEN 8
                    WHEN duration_ms <= 30000 THEN 9
                    ELSE 10
                END AS bucket
            FROM mcp_tool_calls
            WHERE ts > now() - ($1::int * interval '1 minute')
              AND ($2::text IS NULL OR tool = $2)
              AND ($3::text IS NULL OR client_name = $3)
              AND ($4::text IS NULL OR project = $4)
        )
        SELECT tool, bucket, COUNT(*)::bigint
        FROM bucketed
        GROUP BY tool, bucket
        ORDER BY tool, bucket",
    )
    .bind(since_minutes)
    .bind(params.tool.as_deref())
    .bind(params.client_name.as_deref())
    .bind(params.project.as_deref())
    .fetch_all(pool)
    .await
    .map_err(map_db_err)?;

    let labels = [
        "≤1ms", "≤3ms", "≤10ms", "≤30ms", "≤100ms", "≤300ms", "≤1s", "≤3s", "≤10s", "≤30s", ">30s",
    ];
    Ok(json!({
        "buckets": labels,
        "rows": rows.into_iter().map(|(t, b, c)| json!({
            "tool": t, "bucket": b, "label": labels.get(b as usize).copied().unwrap_or("?"), "count": c,
        })).collect::<Vec<_>>(),
    }))
}

async fn agg_raw(
    pool: &sqlx::PgPool,
    params: &McpToolTelemetryParams,
    since_minutes: i32,
) -> Result<Value, McpError> {
    let limit = params.limit.unwrap_or(100).clamp(1, 1000);
    #[allow(clippy::type_complexity)]
    let rows: Vec<(
        i64,
        chrono::DateTime<chrono::Utc>,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        i32,
        String,
        Option<String>,
        Option<String>,
    )> = sqlx::query_as(
        "SELECT id, ts, tool, client_name, client_version, protocol_version,
                mcp_session_id, project, duration_ms, outcome, error_class, request_id
         FROM mcp_tool_calls
         WHERE ts > now() - ($1::int * interval '1 minute')
           AND ($2::text IS NULL OR tool = $2)
           AND ($3::text IS NULL OR client_name = $3)
           AND ($4::text IS NULL OR project = $4)
         ORDER BY ts DESC
         LIMIT $5",
    )
    .bind(since_minutes)
    .bind(params.tool.as_deref())
    .bind(params.client_name.as_deref())
    .bind(params.project.as_deref())
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(map_db_err)?;

    Ok(json!({
        "rows": rows.into_iter().map(|(
            id, ts, tool, client, client_v, proto_v, sess, project, duration, outcome, err_class, req_id,
        )| json!({
            "id": id,
            "ts": ts.to_rfc3339(),
            "tool": tool,
            "client_name": client,
            "client_version": client_v,
            "protocol_version": proto_v,
            "mcp_session_id": sess,
            "project": project,
            "duration_ms": duration,
            "outcome": outcome,
            "error_class": err_class,
            "request_id": req_id,
        })).collect::<Vec<_>>(),
    }))
}

fn map_db_err(e: sqlx::Error) -> McpError {
    error!(error = %e, "mcp_tool_telemetry SQL failed");
    McpError::internal_error(format!("Telemetry query failed: {}", e), None)
}
