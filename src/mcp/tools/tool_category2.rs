//! Categorical constructions over the ProjectDep + Containment categories
//! (ADR-028, item 4): `common_dependency` (pullback — shared dependencies),
//! `integration_point` (pushout — shared dependents), and `functorial_impact`
//! (where the intensive rollup functor loses information vs a size-weighted
//! aggregate). All read existing tables; no new clustering.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::server::{CommonDependencyParams, FunctorialImpactParams, IntegrationPointParams};
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

/// `common_dependency` — the **pullback** of two projects over the dependency
/// relation: projects that BOTH depend on (shared upstream).
pub async fn tool_common_dependency(
    ctx: &SystemContext,
    params: CommonDependencyParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let a = project_id_or_err(ctx, &params.project_a).await?;
    let b = project_id_or_err(ctx, &params.project_b).await?;
    let limit = params.limit.unwrap_or(100).clamp(1, 1000);
    let rows = sqlx::query_as::<_, (String,)>(
        "SELECT p.name FROM projects p
          WHERE p.id IN (SELECT dependency_project_id FROM project_dependencies
                          WHERE dependent_project_id = $1 AND valid_to IS NULL)
            AND p.id IN (SELECT dependency_project_id FROM project_dependencies
                          WHERE dependent_project_id = $2 AND valid_to IS NULL)
          ORDER BY p.name LIMIT $3",
    )
    .bind(a)
    .bind(b)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("pullback: {e}"), None))?;
    let common: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
    json_result(&json!({
        "construction": "pullback (shared dependencies)",
        "project_a": params.project_a,
        "project_b": params.project_b,
        "count": common.len(),
        "common_dependencies": common,
    }))
}

/// `integration_point` — the **pushout** of two projects over the dependency
/// relation: projects that depend on BOTH (shared downstream / integrators).
pub async fn tool_integration_point(
    ctx: &SystemContext,
    params: IntegrationPointParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let a = project_id_or_err(ctx, &params.project_a).await?;
    let b = project_id_or_err(ctx, &params.project_b).await?;
    let limit = params.limit.unwrap_or(100).clamp(1, 1000);
    let rows = sqlx::query_as::<_, (String,)>(
        "SELECT p.name FROM projects p
          WHERE p.id IN (SELECT dependent_project_id FROM project_dependencies
                          WHERE dependency_project_id = $1 AND valid_to IS NULL)
            AND p.id IN (SELECT dependent_project_id FROM project_dependencies
                          WHERE dependency_project_id = $2 AND valid_to IS NULL)
          ORDER BY p.name LIMIT $3",
    )
    .bind(a)
    .bind(b)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("pushout: {e}"), None))?;
    let integrators: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
    json_result(&json!({
        "construction": "pushout (shared dependents / integrators)",
        "project_a": params.project_a,
        "project_b": params.project_b,
        "count": integrators.len(),
        "integration_points": integrators,
    }))
}

/// `functorial_impact` — for each group, the gap between the **intensive** (lax)
/// unweighted mean stored by the rollup and a size-weighted mean. A large gap
/// means collapsing the level (group ← projects) loses information the lax
/// functor doesn't preserve — the metric is misleading at that level.
pub async fn tool_functorial_impact(
    ctx: &SystemContext,
    params: FunctorialImpactParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(50).clamp(1, 500);
    // Per group: stored (unweighted) avg_instability vs file-count-weighted mean.
    let rows = sqlx::query_as::<_, (i64, Option<String>, f64, Option<f64>)>(
        "SELECT g.id, g.label, g.avg_instability,
                (SELECT SUM(pm.avg_instability * pm.file_count)::float8
                        / NULLIF(SUM(pm.file_count), 0)
                   FROM project_group_members m
                   JOIN project_metrics pm ON pm.project_id = m.project_id
                  WHERE m.group_id = g.id AND m.valid_to IS NULL) AS weighted
           FROM hier_group_metrics g
          WHERE g.level = 'group'",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("functorial_impact: {e}"), None))?;

    let mut impacts: Vec<_> = rows
        .into_iter()
        .filter_map(|(id, label, unweighted, weighted)| {
            weighted.map(|w| {
                let delta = (unweighted - w).abs();
                json!({"group_id": id, "label": label, "unweighted_mean": unweighted,
                       "weighted_mean": w, "abs_gap": delta})
            })
        })
        .collect();
    impacts.sort_by(|a, b| {
        b["abs_gap"]
            .as_f64()
            .unwrap_or(0.0)
            .partial_cmp(&a["abs_gap"].as_f64().unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    impacts.truncate(limit as usize);
    json_result(&json!({
        "metric": "avg_instability",
        "law": "lax (intensive mean — not composition-preserving)",
        "count": impacts.len(),
        "impacts": impacts,
        "note": "abs_gap = |unweighted mean (stored) − file-count-weighted mean|. Large gaps mark \
    levels where the intensive rollup is misleading (a few large projects dominate).",
    }))
}
