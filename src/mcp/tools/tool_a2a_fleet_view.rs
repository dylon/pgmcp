//! `a2a_fleet_view` — one read-only view of the agent fleet (Crucible E3).
//!
//! Joins the A2A peer registry (`a2a_agents`) with each agent's trust prior
//! (`agent_trust`), its outcome history (`agent_outcomes` — success rate + most
//! recent outcome), and whether it is currently a live MCP client. Answers, in a
//! single call, the orchestrator's "who is in the fleet, who is trusted, how have
//! they performed, who is live" — the data behind `orchestrator_recommend_next`.
//!
//! Strictly read-only (SELECTs only); no file or state mutation.

use std::sync::atomic::Ordering;

use chrono::{DateTime, Utc};
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde::Serialize;

use crate::context::SystemContext;
use crate::mcp::server::A2aFleetViewParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

/// One fleet member: registry identity + trust + outcome aggregates + liveness.
#[derive(Debug, Serialize, sqlx::FromRow)]
struct FleetRow {
    agent_name: String,
    url: String,
    specialty: Vec<String>,
    recommended_role: Option<String>,
    registered_last_seen: Option<DateTime<Utc>>,
    /// `agent_trust` Bayesian importance prior in [0,1] (anti-flooding prior).
    importance_prior: Option<f32>,
    reports_total: Option<i64>,
    reports_promoted: Option<i64>,
    /// Outcome counts from `agent_outcomes`.
    reports: Option<i64>,
    /// Fraction of outcomes that were `worked`/`prefer`.
    success_rate: Option<f64>,
    last_task_kind: Option<String>,
    last_approach: Option<String>,
    last_outcome: Option<String>,
    last_outcome_at: Option<DateTime<Utc>>,
    /// True iff the agent is also a currently-alive MCP client.
    live_mcp_client: Option<bool>,
    mcp_last_seen: Option<DateTime<Utc>>,
}

pub async fn tool_a2a_fleet_view(
    ctx: &SystemContext,
    params: A2aFleetViewParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(200).clamp(1, 10_000);

    let rows = sqlx::query_as::<_, FleetRow>(
        r#"
        WITH agg AS (
            SELECT agent_id,
                   COUNT(*)                                                        AS reports,
                   COUNT(*) FILTER (WHERE outcome IN ('worked', 'prefer'))::float8
                       / NULLIF(COUNT(*), 0)                                        AS success_rate
            FROM agent_outcomes
            GROUP BY agent_id
        ),
        last_out AS (
            SELECT DISTINCT ON (agent_id)
                   agent_id, task_kind, approach, outcome::text AS outcome, created_at
            FROM agent_outcomes
            ORDER BY agent_id, created_at DESC
        )
        SELECT a.name                AS agent_name,
               a.url                 AS url,
               a.specialty           AS specialty,
               a.recommended_role    AS recommended_role,
               a.last_seen_at        AS registered_last_seen,
               t.importance_prior    AS importance_prior,
               t.reports_total       AS reports_total,
               t.reports_promoted    AS reports_promoted,
               agg.reports           AS reports,
               agg.success_rate      AS success_rate,
               lo.task_kind          AS last_task_kind,
               lo.approach           AS last_approach,
               lo.outcome            AS last_outcome,
               lo.created_at         AS last_outcome_at,
               c.alive               AS live_mcp_client,
               c.last_seen           AS mcp_last_seen
        FROM a2a_agents a
        LEFT JOIN agent_trust t ON t.agent_id = a.name
        LEFT JOIN agg           ON agg.agent_id = a.name
        LEFT JOIN last_out lo   ON lo.agent_id = a.name
        LEFT JOIN mcp_clients c  ON c.client_name = a.name AND c.alive
        ORDER BY a.last_seen_at DESC NULLS LAST, a.name
        LIMIT $1
        "#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("fleet_view query failed: {e}"), None))?;

    let live = rows
        .iter()
        .filter(|r| r.live_mcp_client.unwrap_or(false))
        .count();
    json_result(&serde_json::json!({
        "total": rows.len(),
        "live_mcp_clients": live,
        "agents": rows,
    }))
}
