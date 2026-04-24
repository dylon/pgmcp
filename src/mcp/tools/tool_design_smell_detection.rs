//! `tool_design_smell_detection` — MCP tool body, extracted from `super::super::server`.

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

pub async fn tool_design_smell_detection(
    ctx: &SystemContext,
    params: DesignSmellDetectionParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .design_smell_scans
        .fetch_add(1, Ordering::Relaxed);

    let limit = params.limit.unwrap_or(30);
    let detect_all = params.smells.is_none();
    let smells = params.smells.unwrap_or_default();

    info!(
        tool = "design_smell_detection",
        project = %params.project,
        limit,
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
         JOIN projects p ON f.project_id = p.id
         WHERE p.name = $1",
    )
    .bind(&params.project)
    .fetch_all(
        ctx.db()
            .pool()
            .expect("inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>"),
    )
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
         JOIN projects p ON f.project_id = p.id
         WHERE p.name = $1
         GROUP BY f.relative_path",
    )
    .bind(&params.project)
    .fetch_all(
        ctx.db()
            .pool()
            .expect("inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>"),
    )
    .await
    .unwrap_or_default();

    let topic_map: std::collections::HashMap<&str, i64> = topic_counts
        .iter()
        .map(|r| (r.relative_path.as_str(), r.topic_count))
        .collect();

    // Get co-change partner counts
    let coupling_pairs = ctx
        .db()
        .find_coupled_files(&params.project, 0.2, 2)
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

    let result = serde_json::json!({
        "project": params.project,
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
