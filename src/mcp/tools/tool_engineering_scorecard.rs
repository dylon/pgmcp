//! `tool_engineering_scorecard` — MCP tool body.
//!
//! The 10-dimension scoring + ORR checklist is factored into
//! [`collect_engineering_analysis`] so both this tool and the `quality_report`
//! Engineering pillar share one implementation. `letter_grade`/`grade_gpa` now
//! live in `crate::quality::report` (single source of truth).

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
use crate::mcp::tools::sota_helpers::{pool_or_err, project_id_or_err};
use crate::quality::report::{DimensionScore, OrrGate, grade_gpa, letter_grade};

/// The Engineering pillar's full analysis: 10 graded dimensions, the 8-gate ORR
/// checklist, and the project-stats summary the scorecard header reports.
pub(crate) struct EngineeringAnalysis {
    pub dimensions: Vec<DimensionScore>,
    pub orr: Vec<OrrGate>,
    pub file_count: i64,
    pub total_lines: i64,
    pub avg_file_lines: f64,
    pub test_count: i64,
    pub doc_count: i64,
}

#[derive(sqlx::FromRow)]
struct ProjectStats {
    file_count: i64,
    total_lines: i64,
    avg_file_lines: f64,
}

/// Compute the Engineering analysis for a project. Shared by the
/// `engineering_scorecard` tool and the `quality_report` Engineering pillar.
pub(crate) async fn collect_engineering_analysis(
    ctx: &SystemContext,
    project_id: i32,
    project_name: &str,
) -> Result<EngineeringAnalysis, McpError> {
    let pool = pool_or_err(ctx)?;

    // === Dimension 1: Code Size & Structure ===
    let stats: Option<ProjectStats> = sqlx::query_as::<_, ProjectStats>(
        "SELECT COUNT(*) as file_count, COALESCE(SUM(line_count),0)::BIGINT as total_lines,
                COALESCE(AVG(line_count),0)::DOUBLE PRECISION as avg_file_lines
         FROM indexed_files WHERE project_id = $1",
    )
    .bind(project_id)
    .fetch_optional(pool)
    .await
    .unwrap_or(None);
    let stats = stats.unwrap_or(ProjectStats {
        file_count: 0,
        total_lines: 0,
        avg_file_lines: 0.0,
    });
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
    .fetch_one(pool)
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
    .fetch_one(pool)
    .await
    .unwrap_or(0);
    let test_ratio = if stats.file_count > 0 {
        test_count as f64 / stats.file_count as f64
    } else {
        0.0
    };
    let test_score = (test_ratio * 5.0).min(1.0) * 100.0;

    // === Dimension 4: Documentation ===
    let doc_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM indexed_files WHERE project_id = $1 AND language = 'markdown'",
    )
    .bind(project_id)
    .fetch_one(pool)
    .await
    .unwrap_or(0);
    let doc_score = (doc_count as f64 / stats.file_count.max(1) as f64 * 10.0).min(1.0) * 100.0;

    // === Dimension 5: Code Churn ===
    let avg_churn: Option<f64> = sqlx::query_scalar(
        "SELECT AVG(churn_rate)::DOUBLE PRECISION FROM file_metrics WHERE project_id = $1",
    )
    .bind(project_id)
    .fetch_optional(pool)
    .await
    .unwrap_or(None)
    .flatten();
    let churn_score = (1.0 - avg_churn.unwrap_or(0.0).min(5.0) / 5.0) * 100.0;

    // === Dimension 6: Bug Fix Ratio ===
    let avg_fix: Option<f64> = sqlx::query_scalar(
        "SELECT AVG(fix_commit_ratio)::DOUBLE PRECISION FROM file_metrics WHERE project_id = $1",
    )
    .bind(project_id)
    .fetch_optional(pool)
    .await
    .unwrap_or(None)
    .flatten();
    let fix_score = (1.0 - avg_fix.unwrap_or(0.0) * 3.0).max(0.0) * 100.0;

    // === Dimension 7: Coupling ===
    let avg_coupling: Option<f64> = sqlx::query_scalar(
        "SELECT AVG(COALESCE(afferent_coupling,0) + COALESCE(efferent_coupling,0))::DOUBLE PRECISION
         FROM file_metrics WHERE project_id = $1",
    )
    .bind(project_id)
    .fetch_optional(pool)
    .await
    .unwrap_or(None)
    .flatten();
    let coupling_score = (1.0 - avg_coupling.unwrap_or(0.0).min(20.0) / 20.0) * 100.0;

    // === Dimension 8: Complexity (real per-function cyclomatic) ===
    // A file is "overly complex" when its worst function exceeds an absolute
    // McCabe high-risk threshold (cyclomatic > 15) — criterion-referenced, not a
    // per-project curve. When per-function metrics have not been computed yet,
    // the dimension is N/A (absent) rather than silently falling back to a
    // line-count proxy presented as if it were real complexity.
    #[derive(sqlx::FromRow)]
    struct CycStats {
        files_with_fns: i64,
        complex_files: i64,
    }
    let cyc = sqlx::query_as::<_, CycStats>(
        "SELECT
            COUNT(DISTINCT fm.file_id) AS files_with_fns,
            COUNT(DISTINCT fm.file_id) FILTER (WHERE fm.cyclomatic > 15) AS complex_files
         FROM function_metrics fm
         JOIN indexed_files f ON f.id = fm.file_id
         WHERE f.project_id = $1",
    )
    .bind(project_id)
    .fetch_one(pool)
    .await
    .unwrap_or(CycStats {
        files_with_fns: 0,
        complex_files: 0,
    });
    let complexity_dim = if cyc.files_with_fns == 0 {
        DimensionScore::absent(
            "complexity",
            "Absence of high-cyclomatic functions — N/A (per-function metrics not computed)",
        )
    } else {
        let score = (1.0 - cyc.complex_files as f64 / cyc.files_with_fns as f64).max(0.0) * 100.0;
        DimensionScore::present(
            "complexity",
            "Absence of functions with high cyclomatic complexity (>15)",
            score,
        )
    };

    // God-file detection (ORR gate `no_god_files`): an absolute size-outlier bar.
    // The old gate (`>=5 files over 500 lines`) was unachievable for any sizable
    // repo and did not identify genuine outliers. A single grossly oversized file
    // (>2000 lines) is the signal; the gate passes only when none exceed it.
    const GOD_FILE_LINES: i64 = 2000;
    let god_file_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM indexed_files f
         JOIN projects p ON f.project_id = p.id
         WHERE p.name = $1 AND f.line_count > $2",
    )
    .bind(project_name)
    .bind(GOD_FILE_LINES)
    .fetch_one(pool)
    .await
    .unwrap_or(0);

    // === Dimension 9: Team Distribution ===
    let avg_authors: Option<f64> = sqlx::query_scalar(
        "SELECT AVG(author_count)::DOUBLE PRECISION FROM file_metrics WHERE project_id = $1",
    )
    .bind(project_id)
    .fetch_optional(pool)
    .await
    .unwrap_or(None)
    .flatten();
    let team_score = (avg_authors.unwrap_or(1.0).min(4.0) / 4.0 * 100.0).min(100.0);

    // === Dimension 10: Freshness ===
    let avg_stale: Option<f64> = sqlx::query_scalar(
        "SELECT AVG(days_since_last_change)::DOUBLE PRECISION FROM file_metrics
         WHERE project_id = $1 AND days_since_last_change IS NOT NULL",
    )
    .bind(project_id)
    .fetch_optional(pool)
    .await
    .unwrap_or(None)
    .flatten();
    let freshness_score = (1.0 - avg_stale.unwrap_or(0.0).min(365.0) / 365.0) * 100.0;

    let dimensions = vec![
        DimensionScore::present(
            "code_structure",
            "File size distribution and organization",
            size_score,
        ),
        DimensionScore::present(
            "dependency_health",
            "Absence of circular dependencies",
            dep_score,
        ),
        DimensionScore::present("test_quality", "Test file coverage ratio", test_score),
        DimensionScore::present("documentation", "Documentation file presence", doc_score),
        DimensionScore::present("code_stability", "Low change churn rate", churn_score),
        DimensionScore::present("bug_fix_ratio", "Low proportion of fix commits", fix_score),
        DimensionScore::present("coupling", "Low inter-module coupling", coupling_score),
        complexity_dim,
        DimensionScore::present("team_distribution", "Multi-author bus factor", team_score),
        DimensionScore::present("freshness", "Recent activity on files", freshness_score),
    ];

    let orr = vec![
        OrrGate {
            name: "no_circular_deps".into(),
            pass: cycle_count == 0,
        },
        OrrGate {
            name: "test_coverage".into(),
            pass: test_ratio >= 0.1,
        },
        OrrGate {
            name: "has_documentation".into(),
            pass: doc_count > 0,
        },
        OrrGate {
            name: "low_churn".into(),
            pass: avg_churn.unwrap_or(0.0) < 3.0,
        },
        OrrGate {
            name: "low_fix_ratio".into(),
            pass: avg_fix.unwrap_or(0.0) < 0.3,
        },
        OrrGate {
            name: "no_god_files".into(),
            pass: god_file_count == 0,
        },
        // Process/maintenance gate (honest, absolute): a solo repo legitimately
        // scores low here — it is a real continuity signal, kept (not exempted or
        // curved) per the reliability mandate.
        OrrGate {
            name: "bus_factor_ok".into(),
            pass: avg_authors.unwrap_or(1.0) >= 1.5,
        },
        OrrGate {
            name: "recently_maintained".into(),
            pass: avg_stale.unwrap_or(0.0) < 180.0,
        },
    ];

    Ok(EngineeringAnalysis {
        dimensions,
        orr,
        file_count: stats.file_count,
        total_lines: stats.total_lines,
        avg_file_lines: stats.avg_file_lines,
        test_count,
        doc_count,
    })
}

pub async fn tool_engineering_scorecard(
    ctx: &SystemContext,
    params: EngineeringScorecardParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().scorecard_scans.fetch_add(1, Ordering::Relaxed);

    let format = params.format.as_deref().unwrap_or("full");
    debug!(
        tool = "engineering_scorecard",
        project = %params.project,
        format,
        "MCP tool invoked",
    );

    let project_id = project_id_or_err(ctx, &params.project).await?;
    let analysis = collect_engineering_analysis(ctx, project_id, &params.project).await?;

    // Average only the scorable dimensions' continuous GPAs; data-absent dims
    // (e.g. complexity before per-function metrics exist) are N/A and excluded
    // from the denominator rather than counted as 0.
    let dim_gpas: Vec<f64> = analysis.dimensions.iter().filter_map(|d| d.gpa()).collect();
    let gpa: f64 = if dim_gpas.is_empty() {
        0.0
    } else {
        dim_gpas.iter().sum::<f64>() / dim_gpas.len() as f64
    };

    let dim_json: Vec<serde_json::Value> = analysis
        .dimensions
        .iter()
        .map(|d| match d.score {
            Some(score) => json!({
                "dimension": d.name,
                "score": format!("{:.1}", score),
                "grade": letter_grade(score),
                "description": d.description,
            }),
            None => json!({
                "dimension": d.name,
                "score": "N/A",
                "grade": "N/A",
                "description": d.description,
            }),
        })
        .collect();

    let mut orr_obj = serde_json::Map::new();
    for gate in &analysis.orr {
        orr_obj.insert(gate.name.clone(), serde_json::Value::Bool(gate.pass));
    }
    let orr_pass = analysis.orr.iter().all(|g| g.pass);

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

    let effect_breakdown = if let Some(pool) = ctx.db().pool() {
        crate::mcp::tools::sema_helpers::effects::effect_counts(pool, project_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(eff, count)| json!({ "effect": eff, "count": count }))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    let result = json!({
        "project": params.project,
        "gpa": format!("{:.2}", gpa),
        "overall_grade": letter_grade(gpa * 25.0),
        "dimensions": filtered_dims,
        "orr_checklist": serde_json::Value::Object(orr_obj),
        "orr_pass": orr_pass,
        "effect_breakdown": effect_breakdown,
        "project_stats": {
            "files": analysis.file_count,
            "lines": analysis.total_lines,
            "avg_file_lines": format!("{:.0}", analysis.avg_file_lines),
            "test_files": analysis.test_count,
            "doc_files": analysis.doc_count,
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
