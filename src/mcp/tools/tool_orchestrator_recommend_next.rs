//! `orchestrator_recommend_next` — advisory policy: rank fleet specialists for a
//! task (Crucible E6).
//!
//! Read-only. Given a target `specialty` (and optional `recommended_role`), scores
//! every matching registered agent by a transparent linear combination of:
//!   • specialty overlap with the request,
//!   • shrinkage-adjusted outcome success rate (`agent_outcomes`),
//!   • the `agent_trust` Bayesian importance prior,
//!   • recency/liveness of the agent.
//! Returns the ranked list with the per-component breakdown plus the top pick.
//!
//! THE TOOL RECOMMENDS; THE ORCHESTRATOR (pi) DECIDES AND ACTS. No state is
//! mutated — the reward loop that updates the inputs runs through the existing
//! `a2a_report_outcome` / `experiment_decide`, not here.

use std::collections::BTreeSet;
use std::sync::atomic::Ordering;

use chrono::{DateTime, Utc};
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde::Serialize;

use crate::context::SystemContext;
use crate::mcp::server::OrchestratorRecommendNextParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

// Scoring weights (sum to 1.0). Documented so the policy is auditable.
const W_SPECIALTY: f64 = 0.45;
const W_SUCCESS: f64 = 0.30;
const W_TRUST: f64 = 0.15;
const W_RECENCY: f64 = 0.10;
/// Shrinkage strength: success rate is pulled toward the 0.5 prior until an agent
/// has accumulated reports (`conf = reports / (reports + SHRINKAGE)`), so a single
/// lucky outcome cannot top the ranking.
const SHRINKAGE: f64 = 5.0;
/// Recency half-life (days) for the liveness term.
const RECENCY_HALF_LIFE_DAYS: f64 = 14.0;

#[derive(Debug, sqlx::FromRow)]
struct CandidateRow {
    name: String,
    specialty: Vec<String>,
    recommended_role: Option<String>,
    last_seen_at: Option<DateTime<Utc>>,
    importance_prior: Option<f32>,
    reports: Option<i64>,
    success_rate: Option<f64>,
}

#[derive(Debug, Serialize)]
struct Ranked {
    agent: String,
    recommended_role: Option<String>,
    score: f64,
    specialty_overlap: f64,
    raw_success_rate: Option<f64>,
    adjusted_success: f64,
    trust_prior: f64,
    recency: f64,
    reports: i64,
    matched_specialties: Vec<String>,
}

fn recency_factor(last_seen: Option<DateTime<Utc>>, now: DateTime<Utc>) -> f64 {
    match last_seen {
        // Registered but never re-seen: small, non-zero prior.
        None => 0.3,
        Some(ts) => {
            let age_days = (now - ts).num_seconds().max(0) as f64 / 86_400.0;
            // Exponential decay with the configured half-life, clamped to [0,1].
            0.5_f64
                .powf(age_days / RECENCY_HALF_LIFE_DAYS)
                .clamp(0.0, 1.0)
        }
    }
}

pub async fn tool_orchestrator_recommend_next(
    ctx: &SystemContext,
    params: OrchestratorRecommendNextParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    if params.specialty.is_empty() {
        return Err(McpError::invalid_params(
            "specialty must be a non-empty list of target specialty tags",
            None,
        ));
    }
    let limit = params.limit.unwrap_or(5).clamp(1, 100);
    let now = Utc::now();

    let rows = sqlx::query_as::<_, CandidateRow>(
        r#"
        WITH agg AS (
            SELECT agent_id,
                   COUNT(*)                                                        AS reports,
                   COUNT(*) FILTER (WHERE outcome IN ('worked', 'prefer'))::float8
                       / NULLIF(COUNT(*), 0)                                        AS success_rate
            FROM agent_outcomes
            GROUP BY agent_id
        )
        SELECT a.name             AS name,
               a.specialty        AS specialty,
               a.recommended_role AS recommended_role,
               a.last_seen_at     AS last_seen_at,
               t.importance_prior AS importance_prior,
               agg.reports        AS reports,
               agg.success_rate   AS success_rate
        FROM a2a_agents a
        LEFT JOIN agent_trust t ON t.agent_id = a.name
        LEFT JOIN agg           ON agg.agent_id = a.name
        WHERE a.specialty && $1::text[]
          AND ($2::text IS NULL OR a.recommended_role = $2)
        "#,
    )
    .bind(&params.specialty)
    .bind(&params.recommended_role)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("recommend query failed: {e}"), None))?;

    let want: BTreeSet<&str> = params.specialty.iter().map(|s| s.as_str()).collect();
    let want_n = want.len() as f64;

    let mut ranked: Vec<Ranked> = rows
        .into_iter()
        .map(|r| {
            let matched: Vec<String> = r
                .specialty
                .iter()
                .filter(|s| want.contains(s.as_str()))
                .cloned()
                .collect();
            let specialty_overlap = matched.len() as f64 / want_n;

            let reports = r.reports.unwrap_or(0).max(0);
            let raw_success = r.success_rate;
            // Shrink toward the 0.5 prior by report volume.
            let conf = reports as f64 / (reports as f64 + SHRINKAGE);
            let adjusted_success = 0.5 + (raw_success.unwrap_or(0.5) - 0.5) * conf;

            let trust = r.importance_prior.map(|p| p as f64).unwrap_or(0.5);
            let recency = recency_factor(r.last_seen_at, now);

            let score = W_SPECIALTY * specialty_overlap
                + W_SUCCESS * adjusted_success
                + W_TRUST * trust
                + W_RECENCY * recency;

            Ranked {
                agent: r.name,
                recommended_role: r.recommended_role,
                score,
                specialty_overlap,
                raw_success_rate: raw_success,
                adjusted_success,
                trust_prior: trust,
                recency,
                reports,
                matched_specialties: matched,
            }
        })
        .collect();

    ranked.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let considered = ranked.len();
    ranked.truncate(limit as usize);
    let recommended = ranked.first().map(|r| r.agent.clone());

    json_result(&serde_json::json!({
        "task": params.task,
        "specialty": params.specialty,
        "recommended_role": params.recommended_role,
        "candidates_considered": considered,
        "weights": {
            "specialty": W_SPECIALTY, "success": W_SUCCESS,
            "trust": W_TRUST, "recency": W_RECENCY, "shrinkage": SHRINKAGE,
        },
        "recommended": recommended,
        "ranked": ranked,
    }))
}
