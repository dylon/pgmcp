//! `tool_mcp_telemetry` — query the durable `mcp_tool_calls` table.
//!
//! Reports the four cuts the user typically asks about: total calls per
//! tool, latency percentiles, error rate, and breakdowns by caller agent
//! or project. The in-memory `StatsTracker::tool_telemetry` snapshot is
//! the right tool for the "right now" view (real-time, no DB round-trip)
//! and is exposed via `/api/status`. This tool is for the historical
//! lookback — the last hour, the last day — read straight from
//! `mcp_tool_calls` with SQL aggregations.

use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::{Value, json};
use tracing::{debug, error};

use crate::context::SystemContext;
use crate::mcp::server::McpToolTelemetryParams;

#[derive(Debug)]
struct TelemetryQuery {
    tool: Option<String>,
    client_name: Option<String>,
    project: Option<String>,
    since_minutes: i32,
    raw_limit: i32,
    aggregation: String,
}

fn normalized_optional(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn normalize_params(params: McpToolTelemetryParams) -> TelemetryQuery {
    TelemetryQuery {
        tool: normalized_optional(params.tool),
        client_name: normalized_optional(params.client_name),
        project: normalized_optional(params.project),
        since_minutes: params.since_minutes.unwrap_or(60).clamp(1, 60 * 24 * 31),
        raw_limit: params.limit.unwrap_or(100).clamp(1, 1000),
        aggregation: params
            .aggregation
            .map(|aggregation| aggregation.trim().to_string())
            .filter(|aggregation| !aggregation.is_empty())
            .unwrap_or_else(|| "summary".to_string()),
    }
}

pub async fn tool_mcp_tool_telemetry(
    ctx: &SystemContext,
    params: McpToolTelemetryParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let query = normalize_params(params);
    debug!(
        tool = "mcp_tool_telemetry",
        aggregation = %query.aggregation,
        since_minutes = query.since_minutes,
        "MCP tool invoked"
    );

    let pool = ctx.db().pool().ok_or_else(|| {
        McpError::internal_error("mcp_tool_telemetry requires a real PgPool", None)
    })?;

    let body = match query.aggregation.as_str() {
        "summary" => agg_summary(pool, &query).await?,
        "top_tools" => agg_top_tools(pool, &query).await?,
        "top_callers" => agg_top_callers(pool, &query).await?,
        "top_projects" => agg_top_projects(pool, &query).await?,
        "error_rate" => agg_error_rate(pool, &query).await?,
        "histogram" => agg_histogram(pool, &query).await?,
        "raw" => agg_raw(pool, &query).await?,
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

    // Shadow-ASR channel (Phase D2b): workspace-wide effect distribution.
    let effect_breakdown: Vec<serde_json::Value> = (async {
        let Some(pool) = ctx.db().pool() else {
            return Vec::new();
        };
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT se.effect, COUNT(*)::int8
             FROM symbol_effects se
             GROUP BY se.effect
             ORDER BY se.effect",
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        rows.into_iter()
            .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
            .collect()
    })
    .await;

    let envelope = json!({
        "effect_breakdown": effect_breakdown,
        "aggregation": query.aggregation,
        "since_minutes": query.since_minutes,
        "filters": {
            "tool": query.tool,
            "client_name": query.client_name,
            "project": query.project,
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

async fn agg_summary(pool: &sqlx::PgPool, query: &TelemetryQuery) -> Result<Value, McpError> {
    #[allow(clippy::type_complexity)]
    let rows: Vec<(String, String, Option<String>, i64, i64, f64, f64, f64, f64, i64)> =
        sqlx::query_as(
            "SELECT
                tool,
                client_name,
                NULLIF(project, '')                                      AS project,
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
               AND ($4::text IS NULL OR NULLIF(project, '') = $4)
             GROUP BY tool, client_name, NULLIF(project, '')
             ORDER BY calls DESC
             LIMIT 1000",
        )
        .bind(query.since_minutes)
        .bind(query.tool.as_deref())
        .bind(query.client_name.as_deref())
        .bind(query.project.as_deref())
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

async fn agg_top_tools(pool: &sqlx::PgPool, query: &TelemetryQuery) -> Result<Value, McpError> {
    let rows: Vec<(String, i64, i64, f64)> = sqlx::query_as(
        "SELECT tool, COUNT(*), COUNT(*) FILTER (WHERE outcome <> 'ok'),
                COALESCE(PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY duration_ms), 0.0)::float8 AS p95_ms
         FROM mcp_tool_calls
         WHERE ts > now() - ($1::int * interval '1 minute')
           AND ($2::text IS NULL OR tool = $2)
           AND ($3::text IS NULL OR client_name = $3)
           AND ($4::text IS NULL OR NULLIF(project, '') = $4)
         GROUP BY tool
         ORDER BY COUNT(*) DESC
         LIMIT 50",
    )
    .bind(query.since_minutes)
    .bind(query.tool.as_deref())
    .bind(query.client_name.as_deref())
    .bind(query.project.as_deref())
    .fetch_all(pool)
    .await
    .map_err(map_db_err)?;

    Ok(json!({
        "rows": rows.into_iter().map(|(t, c, e, p)| json!({
            "tool": t, "calls": c, "errors": e, "p95_ms": p,
        })).collect::<Vec<_>>()
    }))
}

async fn agg_top_callers(pool: &sqlx::PgPool, query: &TelemetryQuery) -> Result<Value, McpError> {
    let rows: Vec<(String, i64, i64, i64)> = sqlx::query_as(
        "SELECT client_name, COUNT(*), COUNT(*) FILTER (WHERE outcome <> 'ok'),
                COUNT(DISTINCT tool)
         FROM mcp_tool_calls
         WHERE ts > now() - ($1::int * interval '1 minute')
           AND ($2::text IS NULL OR tool = $2)
           AND ($3::text IS NULL OR client_name = $3)
           AND ($4::text IS NULL OR NULLIF(project, '') = $4)
         GROUP BY client_name
         ORDER BY COUNT(*) DESC
         LIMIT 50",
    )
    .bind(query.since_minutes)
    .bind(query.tool.as_deref())
    .bind(query.client_name.as_deref())
    .bind(query.project.as_deref())
    .fetch_all(pool)
    .await
    .map_err(map_db_err)?;

    Ok(json!({
        "rows": rows.into_iter().map(|(c, n, e, t)| json!({
            "client_name": c, "calls": n, "errors": e, "distinct_tools": t,
        })).collect::<Vec<_>>()
    }))
}

async fn agg_top_projects(pool: &sqlx::PgPool, query: &TelemetryQuery) -> Result<Value, McpError> {
    let rows: Vec<(Option<String>, i64, i64)> = sqlx::query_as(
        "SELECT NULLIF(project, '') AS project, COUNT(*), COUNT(*) FILTER (WHERE outcome <> 'ok')
         FROM mcp_tool_calls
         WHERE ts > now() - ($1::int * interval '1 minute')
           AND NULLIF(project, '') IS NOT NULL
           AND ($2::text IS NULL OR tool = $2)
           AND ($3::text IS NULL OR client_name = $3)
           AND ($4::text IS NULL OR NULLIF(project, '') = $4)
         GROUP BY NULLIF(project, '')
         ORDER BY COUNT(*) DESC
         LIMIT 50",
    )
    .bind(query.since_minutes)
    .bind(query.tool.as_deref())
    .bind(query.client_name.as_deref())
    .bind(query.project.as_deref())
    .fetch_all(pool)
    .await
    .map_err(map_db_err)?;

    Ok(json!({
        "rows": rows.into_iter().map(|(p, n, e)| json!({
            "project": p, "calls": n, "errors": e,
        })).collect::<Vec<_>>()
    }))
}

async fn agg_error_rate(pool: &sqlx::PgPool, query: &TelemetryQuery) -> Result<Value, McpError> {
    let rows: Vec<(String, i64, i64, f64)> = sqlx::query_as(
        "SELECT tool, COUNT(*), COUNT(*) FILTER (WHERE outcome <> 'ok'),
                (COUNT(*) FILTER (WHERE outcome <> 'ok'))::float8 / NULLIF(COUNT(*), 0)::float8 AS error_rate
         FROM mcp_tool_calls
         WHERE ts > now() - ($1::int * interval '1 minute')
           AND ($2::text IS NULL OR tool = $2)
           AND ($3::text IS NULL OR client_name = $3)
           AND ($4::text IS NULL OR NULLIF(project, '') = $4)
         GROUP BY tool
         ORDER BY error_rate DESC NULLS LAST, COUNT(*) DESC
         LIMIT 50",
    )
    .bind(query.since_minutes)
    .bind(query.tool.as_deref())
    .bind(query.client_name.as_deref())
    .bind(query.project.as_deref())
    .fetch_all(pool)
    .await
    .map_err(map_db_err)?;

    Ok(json!({
        "rows": rows.into_iter().map(|(t, c, e, r)| json!({
            "tool": t, "calls": c, "errors": e, "error_rate": r,
        })).collect::<Vec<_>>()
    }))
}

async fn agg_histogram(pool: &sqlx::PgPool, query: &TelemetryQuery) -> Result<Value, McpError> {
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
              AND ($4::text IS NULL OR NULLIF(project, '') = $4)
        )
        SELECT tool, bucket, COUNT(*)::bigint
        FROM bucketed
        GROUP BY tool, bucket
        ORDER BY tool, bucket",
    )
    .bind(query.since_minutes)
    .bind(query.tool.as_deref())
    .bind(query.client_name.as_deref())
    .bind(query.project.as_deref())
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

async fn agg_raw(pool: &sqlx::PgPool, query: &TelemetryQuery) -> Result<Value, McpError> {
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
                mcp_session_id, NULLIF(project, '') AS project, duration_ms, outcome, error_class, request_id
         FROM mcp_tool_calls
         WHERE ts > now() - ($1::int * interval '1 minute')
           AND ($2::text IS NULL OR tool = $2)
           AND ($3::text IS NULL OR client_name = $3)
           AND ($4::text IS NULL OR NULLIF(project, '') = $4)
         ORDER BY ts DESC
         LIMIT $5",
    )
    .bind(query.since_minutes)
    .bind(query.tool.as_deref())
    .bind(query.client_name.as_deref())
    .bind(query.project.as_deref())
    .bind(query.raw_limit)
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
