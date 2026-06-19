//! `tool_design_smell_detection` — MCP tool body, extracted from `super::super::server`.

#![allow(unused_imports)]

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, LoggingLevel};
use serde_json::json;
use tracing::debug;

use crate::context::SystemContext;
use crate::db::queries;
use crate::mcp::server::*;
use crate::mcp::tools::sota_helpers::{pool_or_err, project_id_or_err};

const MAX_DESIGN_SMELL_LIMIT: i32 = 1_000;
const ALLOWED_DESIGN_SMELLS: &[&str] = &[
    "god_class",
    "srp_violation",
    "shotgun_surgery",
    "stale_module",
    "unstable_dependency",
];

fn normalize_design_smells(smells: Option<Vec<String>>) -> Result<(bool, Vec<String>), McpError> {
    let Some(smells) = smells else {
        return Ok((true, Vec::new()));
    };
    if smells.is_empty() {
        return Err(McpError::invalid_params(
            "smells must not be empty when provided",
            None,
        ));
    }

    let mut out = Vec::with_capacity(smells.len());
    for smell in smells {
        let smell = smell.trim();
        if !ALLOWED_DESIGN_SMELLS.contains(&smell) {
            return Err(McpError::invalid_params(
                format!(
                    "smell '{}' is invalid; expected one of: god_class, srp_violation, shotgun_surgery, stale_module, unstable_dependency",
                    smell
                ),
                None,
            ));
        }
        out.push(smell.to_string());
    }
    out.sort();
    out.dedup();
    Ok((false, out))
}

pub async fn tool_design_smell_detection(
    ctx: &SystemContext,
    params: DesignSmellDetectionParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .design_smell_scans
        .fetch_add(1, Ordering::Relaxed);

    let project = params.project.trim().to_string();
    if project.is_empty() {
        return Err(McpError::invalid_params("project must be non-empty", None));
    }
    let limit = params.limit.unwrap_or(30).clamp(1, MAX_DESIGN_SMELL_LIMIT);
    let (detect_all, smells) = normalize_design_smells(params.smells)?;
    let include_fixes = params.include_fixes.unwrap_or(true);
    let pool = pool_or_err(ctx)?;
    let project_id = project_id_or_err(ctx, &project).await?;

    debug!(
        tool = "design_smell_detection",
        project = %project,
        limit,
        include_fixes,
        "MCP tool invoked",
    );

    #[derive(sqlx::FromRow)]
    #[allow(dead_code)]
    struct SmellRow {
        relative_path: String,
        language: String,
        size_bytes: i64,
        line_count: i32,
        pagerank: Option<f64>,
        in_degree: Option<i32>,
        out_degree: Option<i32>,
        commit_count: Option<i32>,
        churn_rate: Option<f64>,
        days_since_last_change: Option<i32>,
    }

    let rows: Vec<SmellRow> = sqlx::query_as::<_, SmellRow>(
        "SELECT f.relative_path, f.language, f.size_bytes, f.line_count,
                fm.pagerank, fm.in_degree, fm.out_degree,
                fm.commit_count, fm.churn_rate, fm.days_since_last_change
         FROM indexed_files f
         LEFT JOIN file_metrics fm ON fm.file_id = f.id
                                  AND fm.project_id = f.project_id
         WHERE f.project_id = $1",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Query failed: {}", e), None))?;

    // Get topic counts per file
    #[derive(sqlx::FromRow)]
    struct TopicCountRow {
        relative_path: String,
        topic_count: i64,
    }

    let topic_counts: Vec<TopicCountRow> = sqlx::query_as::<_, TopicCountRow>(
        "SELECT f.relative_path, COUNT(DISTINCT cta.topic_id) as topic_count
         FROM indexed_files f
         JOIN file_chunks fc ON fc.file_id = f.id
         JOIN chunk_topic_assignments cta ON cta.chunk_id = fc.id
         WHERE f.project_id = $1
         GROUP BY f.relative_path",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    let topic_map: std::collections::HashMap<&str, i64> = topic_counts
        .iter()
        .map(|r| (r.relative_path.as_str(), r.topic_count))
        .collect();

    // Get co-change partner counts
    let coupling_pairs = queries::find_coupled_files_by_project_id(pool, project_id, 0.2, 2)
        .await
        .unwrap_or_default();

    let mut coupling_count: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for pair in &coupling_pairs {
        *coupling_count.entry(pair.file_a.clone()).or_insert(0) += 1;
        *coupling_count.entry(pair.file_b.clone()).or_insert(0) += 1;
    }

    let mut detected_smells: Vec<serde_json::Value> = Vec::new();

    for row in &rows {
        let topics = topic_map
            .get(row.relative_path.as_str())
            .copied()
            .unwrap_or(0);
        let partners = coupling_count.get(&row.relative_path).copied().unwrap_or(0);

        // God class: large file with many topics
        if (detect_all || smells.iter().any(|s| s == "god_class"))
            && row.line_count > 500
            && topics > 5
        {
            detected_smells.push(serde_json::json!({
                "smell": "god_class",
                "severity": "high",
                "path": row.relative_path,
                "reason": format!("{} lines, {} topics", row.line_count, topics),
                "line_count": row.line_count,
                "topic_count": topics,
            }));
        }

        // SRP violation: many topics
        if (detect_all || smells.iter().any(|s| s == "srp_violation"))
            && topics > 4
            && row.line_count > 200
        {
            detected_smells.push(serde_json::json!({
                "smell": "srp_violation",
                "severity": "medium",
                "path": row.relative_path,
                "reason": format!("{} distinct topics — file handles too many concerns", topics),
                "topic_count": topics,
            }));
        }

        // Shotgun surgery: many co-change partners
        if (detect_all || smells.iter().any(|s| s == "shotgun_surgery")) && partners > 8 {
            detected_smells.push(serde_json::json!({
                "smell": "shotgun_surgery",
                "severity": "high",
                "path": row.relative_path,
                "reason": format!("{} co-change partners — changes here ripple widely", partners),
                "co_change_partners": partners,
            }));
        }

        // Stale module: old and untouched
        if (detect_all || smells.iter().any(|s| s == "stale_module"))
            && let Some(days) = row.days_since_last_change
            && days > 365
            && row.line_count > 100
        {
            detected_smells.push(serde_json::json!({
                "smell": "stale_module",
                "severity": "low",
                "path": row.relative_path,
                "reason": format!("Unchanged for {} days ({} lines)", days, row.line_count),
                "days_since_change": days,
            }));
        }

        // Unstable dependency: high churn with many dependents
        if detect_all || smells.iter().any(|s| s == "unstable_dependency") {
            let in_deg = row.in_degree.unwrap_or(0);
            let churn = row.churn_rate.unwrap_or(0.0);
            if in_deg > 5 && churn > 2.0 {
                detected_smells.push(serde_json::json!({
                    "smell": "unstable_dependency",
                    "severity": "high",
                    "path": row.relative_path,
                    "reason": format!("{} dependents but churn rate {:.1}/month — unstable core dependency",
                        in_deg, churn),
                    "in_degree": in_deg,
                    "churn_rate": format!("{:.1}", churn),
                }));
            }
        }
    }

    // Sort by severity descending
    let severity_order = |s: &str| -> i32 {
        match s {
            "high" => 3,
            "medium" => 2,
            "low" => 1,
            _ => 0,
        }
    };
    detected_smells.sort_by(|a, b| {
        let sa = severity_order(a["severity"].as_str().unwrap_or("low"));
        let sb = severity_order(b["severity"].as_str().unwrap_or("low"));
        sb.cmp(&sa)
    });
    detected_smells.truncate(limit as usize);

    // Phase 1 backfill: attach a typed `recommended_fix` to each smell.
    if include_fixes {
        use crate::mcp::tools::fix_helpers::default_fix_for_smell;
        for s in &mut detected_smells {
            let smell_type = match s["smell"].as_str() {
                Some(t) => t.to_string(),
                None => continue,
            };
            let path = s["path"].as_str().unwrap_or("?").to_string();
            // Pull the line_count if present; otherwise default to 0 (the
            // builder clamps to 1 to keep PathRange.end_line valid).
            let line_count = s["line_count"].as_i64().unwrap_or(0).min(i32::MAX as i64) as i32;
            let metric_summary = s["reason"].as_str().unwrap_or("").to_string();
            if let Some(fix) =
                default_fix_for_smell(&smell_type, &project, &path, line_count, &metric_summary)
                && let Ok(fix_json) = serde_json::to_value(&fix)
                && let Some(obj) = s.as_object_mut()
            {
                obj.insert("recommended_fix".to_string(), fix_json);
            }
        }
    }

    // Shadow-ASR channel (Phase D2b): per-effect symbol-count breakdown
    // for the project. Universal enrichment — every tool benefits from
    // surfacing the effect distribution alongside its primary output.
    // Gracefully degrades to empty when the project lookup or
    // shadow-ASR data isn't populated.
    let effect_breakdown: Vec<serde_json::Value> =
        crate::mcp::tools::sema_helpers::effects::effect_counts(pool, project_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
            .collect();

    let result = serde_json::json!({
        "effect_breakdown": effect_breakdown,
        "project": project,
        "limit": limit,
        "detect_all": detect_all,
        "smells_requested": smells,
        "smell_count": detected_smells.len(),
        "smells": detected_smells,
        "guidance": "God classes and SRP violations should be split. Shotgun surgery files \
                     need interface stabilization. Stale modules may be dead code. \
                     Unstable dependencies need refactoring to reduce churn.",
    });

    let json = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "design_smell_detection",
        smells = detected_smells.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
