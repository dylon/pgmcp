//! `tool_engineering_scorecard` — MCP tool body, extracted from `super::super::server`.

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

pub async fn tool_engineering_scorecard(
    ctx: &SystemContext,
    params: EngineeringScorecardParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().scorecard_scans.fetch_add(1, Ordering::Relaxed);

    let format = params.format.as_deref().unwrap_or("full");

    info!(
        tool = "engineering_scorecard",
        project = %params.project,
        format,
        "MCP tool invoked",
    );

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

    fn letter_grade(score: f64) -> &'static str {
        if score >= 90.0 {
            "A"
        } else if score >= 80.0 {
            "B"
        } else if score >= 70.0 {
            "C"
        } else if score >= 60.0 {
            "D"
        } else {
            "F"
        }
    }
    fn grade_gpa(grade: &str) -> f64 {
        match grade {
            "A" => 4.0,
            "B" => 3.0,
            "C" => 2.0,
            "D" => 1.0,
            _ => 0.0,
        }
    }

    // === Dimension 1: Code Size & Structure ===
    #[derive(sqlx::FromRow)]
    struct ProjectStats {
        file_count: i64,
        total_lines: i64,
        avg_file_lines: f64,
    }

    let stats: Option<ProjectStats> = sqlx::query_as::<_, ProjectStats>(
        "SELECT COUNT(*) as file_count, SUM(line_count)::BIGINT as total_lines,
                AVG(line_count)::DOUBLE PRECISION as avg_file_lines
         FROM indexed_files WHERE project_id = $1",
    )
    .bind(project_id)
    .fetch_optional(
        ctx.db()
            .pool()
            .expect("inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>"),
    )
    .await
    .unwrap_or(None);

    let stats = stats.unwrap_or(ProjectStats {
        file_count: 0,
        total_lines: 0,
        avg_file_lines: 0.0,
    });
    // Good avg file size: 100-300 lines. Penalize >500 avg
    let size_score = (1.0 - (stats.avg_file_lines - 200.0).abs().max(0.0) / 800.0).max(0.0) * 100.0;

    // === Dimension 2: Dependency Health ===
    let cycle_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM (
            SELECT DISTINCT e1.source_file_id
            FROM code_graph_edges e1
            JOIN code_graph_edges e2 ON e1.target_file_id = e2.source_file_id
                AND e2.target_file_id = e1.source_file_id
            WHERE e1.project_id = $1 AND e1.edge_type = 'import'
                AND e2.edge_type = 'import'
        ) t",
    )
    .bind(project_id)
    .fetch_one(
        ctx.db()
            .pool()
            .expect("inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>"),
    )
    .await
    .unwrap_or(0);
    let dep_score = if stats.file_count > 0 {
        (1.0 - cycle_count as f64 / stats.file_count as f64).max(0.0) * 100.0
    } else {
        100.0
    };

    // === Dimension 3: Test Quality ===
    let test_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM indexed_files
         WHERE project_id = $1 AND relative_path ~* '(test|spec|_test\\.|_spec\\.)'",
    )
    .bind(project_id)
    .fetch_one(
        ctx.db()
            .pool()
            .expect("inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>"),
    )
    .await
    .unwrap_or(0);
    let test_ratio = if stats.file_count > 0 {
        test_count as f64 / stats.file_count as f64
    } else {
        0.0
    };
    let test_score = (test_ratio * 5.0).min(1.0) * 100.0; // 20% test files = 100

    // === Dimension 4: Documentation ===
    let doc_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM indexed_files WHERE project_id = $1 AND language = 'markdown'",
    )
    .bind(project_id)
    .fetch_one(
        ctx.db()
            .pool()
            .expect("inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>"),
    )
    .await
    .unwrap_or(0);
    let doc_score = (doc_count as f64 / stats.file_count.max(1) as f64 * 10.0).min(1.0) * 100.0;

    // === Dimension 5: Code Churn ===
    let avg_churn: Option<f64> = sqlx::query_scalar(
        "SELECT AVG(churn_rate)::DOUBLE PRECISION FROM file_metrics WHERE project_id = $1",
    )
    .bind(project_id)
    .fetch_optional(
        ctx.db()
            .pool()
            .expect("inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>"),
    )
    .await
    .unwrap_or(None)
    .flatten();
    let churn_score = (1.0 - avg_churn.unwrap_or(0.0).min(5.0) / 5.0) * 100.0;

    // === Dimension 6: Bug Fix Ratio ===
    let avg_fix: Option<f64> = sqlx::query_scalar(
        "SELECT AVG(fix_commit_ratio)::DOUBLE PRECISION FROM file_metrics WHERE project_id = $1",
    )
    .bind(project_id)
    .fetch_optional(
        ctx.db()
            .pool()
            .expect("inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>"),
    )
    .await
    .unwrap_or(None)
    .flatten();
    let fix_score = (1.0 - avg_fix.unwrap_or(0.0) * 3.0).max(0.0) * 100.0;

    // === Dimension 7: Coupling ===
    let avg_coupling: Option<f64> = sqlx::query_scalar(
        "SELECT AVG(COALESCE(afferent_coupling,0) + COALESCE(efferent_coupling,0))::DOUBLE PRECISION
         FROM file_metrics WHERE project_id = $1"
    )
    .bind(project_id)
    .fetch_optional(ctx.db().pool().expect("inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>"))
    .await
    .unwrap_or(None)
    .flatten();
    let coupling_score = (1.0 - avg_coupling.unwrap_or(0.0).min(20.0) / 20.0) * 100.0;

    // === Dimension 8: Complexity ===
    let high_complexity_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM indexed_files f
         JOIN projects p ON f.project_id = p.id
         WHERE p.name = $1 AND f.line_count > 500",
    )
    .bind(&params.project)
    .fetch_one(
        ctx.db()
            .pool()
            .expect("inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>"),
    )
    .await
    .unwrap_or(0);
    let complexity_score = if stats.file_count > 0 {
        (1.0 - high_complexity_count as f64 / stats.file_count as f64).max(0.0) * 100.0
    } else {
        100.0
    };

    // === Dimension 9: Team Distribution ===
    let avg_authors: Option<f64> = sqlx::query_scalar(
        "SELECT AVG(author_count)::DOUBLE PRECISION FROM file_metrics WHERE project_id = $1",
    )
    .bind(project_id)
    .fetch_optional(
        ctx.db()
            .pool()
            .expect("inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>"),
    )
    .await
    .unwrap_or(None)
    .flatten();
    // Bus factor: higher avg authors = better. 2+ is good.
    let team_score = (avg_authors.unwrap_or(1.0).min(4.0) / 4.0 * 100.0).min(100.0);

    // === Dimension 10: Freshness ===
    let avg_stale: Option<f64> = sqlx::query_scalar(
        "SELECT AVG(days_since_last_change)::DOUBLE PRECISION FROM file_metrics
         WHERE project_id = $1 AND days_since_last_change IS NOT NULL",
    )
    .bind(project_id)
    .fetch_optional(
        ctx.db()
            .pool()
            .expect("inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>"),
    )
    .await
    .unwrap_or(None)
    .flatten();
    let freshness_score = (1.0 - avg_stale.unwrap_or(0.0).min(365.0) / 365.0) * 100.0;

    let dimensions = vec![
        (
            "code_structure",
            size_score,
            "File size distribution and organization",
        ),
        (
            "dependency_health",
            dep_score,
            "Absence of circular dependencies",
        ),
        ("test_quality", test_score, "Test file coverage ratio"),
        ("documentation", doc_score, "Documentation file presence"),
        ("code_stability", churn_score, "Low change churn rate"),
        ("bug_fix_ratio", fix_score, "Low proportion of fix commits"),
        ("coupling", coupling_score, "Low inter-module coupling"),
        (
            "complexity",
            complexity_score,
            "Absence of overly complex files",
        ),
        ("team_distribution", team_score, "Multi-author bus factor"),
        ("freshness", freshness_score, "Recent activity on files"),
    ];

    let gpa: f64 = dimensions
        .iter()
        .map(|(_, s, _)| grade_gpa(letter_grade(*s)))
        .sum::<f64>()
        / dimensions.len() as f64;

    let dim_json: Vec<serde_json::Value> = dimensions
        .iter()
        .map(|(name, score, desc)| {
            let grade = letter_grade(*score);
            serde_json::json!({
                "dimension": name,
                "score": format!("{:.1}", score),
                "grade": grade,
                "description": desc,
            })
        })
        .collect();

    // ORR checklist
    let orr = serde_json::json!({
        "no_circular_deps": cycle_count == 0,
        "test_coverage": test_ratio >= 0.1,
        "has_documentation": doc_count > 0,
        "low_churn": avg_churn.unwrap_or(0.0) < 3.0,
        "low_fix_ratio": avg_fix.unwrap_or(0.0) < 0.3,
        "no_god_files": high_complexity_count < 5,
        "bus_factor_ok": avg_authors.unwrap_or(1.0) >= 1.5,
        "recently_maintained": avg_stale.unwrap_or(0.0) < 180.0,
    });

    let orr_pass = orr
        .as_object()
        .map(|o| o.values().all(|v| v.as_bool().unwrap_or(false)))
        .unwrap_or(false);

    // Filter for failures_only
    let filtered_dims = if format == "failures_only" {
        dim_json
            .iter()
            .filter(|d| {
                let grade = d["grade"].as_str().unwrap_or("A");
                grade == "C" || grade == "D" || grade == "F"
            })
            .cloned()
            .collect::<Vec<_>>()
    } else {
        dim_json
    };

    let result = serde_json::json!({
        "project": params.project,
        "gpa": format!("{:.2}", gpa),
        "overall_grade": letter_grade(gpa * 25.0),
        "dimensions": filtered_dims,
        "orr_checklist": orr,
        "orr_pass": orr_pass,
        "project_stats": {
            "files": stats.file_count,
            "lines": stats.total_lines,
            "avg_file_lines": format!("{:.0}", stats.avg_file_lines),
            "test_files": test_count,
            "doc_files": doc_count,
        },
        "guidance": if orr_pass {
            "Project passes Operational Readiness Review. Focus on improving dimensions with grade C or below."
        } else {
            "Project does NOT pass ORR. Address failing checklist items before deployment."
        },
    });

    let json = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "engineering_scorecard",
        gpa = %format!("{:.2}", gpa),
        orr_pass,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
