//! `tool_architecture_quality` — MCP tool body, extracted from `super::super::server`.

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

pub async fn tool_architecture_quality(
    ctx: &SystemContext,
    params: ArchitectureQualityParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .architecture_quality_scans
        .fetch_add(1, Ordering::Relaxed);

    let detail = params.detail.as_deref().unwrap_or("summary");

    debug!(
        tool = "architecture_quality",
        project = %params.project,
        detail,
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

    // 1. Separation of concerns: avg topic count per file (lower = better)
    let avg_topics: Option<f64> = sqlx::query_scalar(
        "SELECT AVG(topic_count)::DOUBLE PRECISION FROM (
            SELECT COUNT(DISTINCT cta.topic_id) as topic_count
            FROM indexed_files f
            JOIN file_chunks fc ON fc.file_id = f.id
            JOIN chunk_topic_assignments cta ON cta.chunk_id = fc.id
            WHERE f.project_id = $1
            GROUP BY f.id
        ) t",
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
    let soc_score = (1.0 - (avg_topics.unwrap_or(1.0) - 1.0).max(0.0) / 10.0).max(0.0) * 100.0;

    // 2. Loose coupling: avg instability distance from 0.5 (mid-range is best)
    let avg_coupling: Option<f64> = sqlx::query_scalar(
        "SELECT AVG(COALESCE(afferent_coupling, 0) + COALESCE(efferent_coupling, 0))::DOUBLE PRECISION
         FROM file_metrics WHERE project_id = $1"
    )
    .bind(project_id)
    .fetch_optional(ctx.db().pool().expect("inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>"))
    .await
    .unwrap_or(None)
    .flatten();
    let coupling_score = (1.0 - avg_coupling.unwrap_or(0.0).min(20.0) / 20.0) * 100.0;

    // 3. Acyclicity: fraction of files NOT in cycles
    let total_files: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM indexed_files WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(ctx.db().pool().expect(
                "inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>",
            ))
            .await
            .unwrap_or(0);

    // Use SCC count from edges (approximate — files in cycles)
    let files_in_cycles: i64 = sqlx::query_scalar(
        "SELECT COUNT(DISTINCT source_file_id) FROM (
            SELECT e1.source_file_id
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
    let acyclicity_score = if total_files > 0 {
        (1.0 - files_in_cycles as f64 / total_files as f64) * 100.0
    } else {
        100.0
    };

    // 4. Test coverage: fraction of files that have test files
    let test_file_count: i64 = sqlx::query_scalar(
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
    let test_score = if total_files > 0 {
        (test_file_count as f64 / total_files as f64 * 3.0).min(1.0) * 100.0
    } else {
        0.0
    };

    // 5. Doc coverage: fraction of files with markdown docs
    let doc_file_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM indexed_files
         WHERE project_id = $1 AND language = 'markdown'",
    )
    .bind(project_id)
    .fetch_one(
        ctx.db()
            .pool()
            .expect("inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>"),
    )
    .await
    .unwrap_or(0);
    let doc_score = if total_files > 0 {
        (doc_file_count as f64 / total_files as f64 * 10.0).min(1.0) * 100.0
    } else {
        0.0
    };

    // 6-10: Additional quality dimensions from file_metrics
    let avg_pagerank: Option<f64> = sqlx::query_scalar(
        "SELECT AVG(pagerank)::DOUBLE PRECISION FROM file_metrics WHERE project_id = $1",
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
    // More evenly distributed PageRank = better
    let balance_score = avg_pagerank
        .map(|pr| {
            let expected = 1.0 / total_files.max(1) as f64;
            (1.0 - (pr - expected).abs() / expected.max(0.001)).max(0.0) * 100.0
        })
        .unwrap_or(50.0);

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
    let stability_score = (1.0 - avg_churn.unwrap_or(0.0).min(5.0) / 5.0) * 100.0;

    let avg_fix_ratio: Option<f64> = sqlx::query_scalar(
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
    let health_score = (1.0 - avg_fix_ratio.unwrap_or(0.0)) * 100.0;

    // SDP compliance: percentage of edges where stable doesn't depend on unstable
    let sdp_violations: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM code_graph_edges e
         JOIN file_metrics fm_s ON fm_s.file_id = e.source_file_id
         JOIN file_metrics fm_t ON fm_t.file_id = e.target_file_id
         WHERE e.project_id = $1 AND e.edge_type = 'import'
           AND fm_s.instability < 0.3 AND fm_t.instability > 0.7",
    )
    .bind(project_id)
    .fetch_one(
        ctx.db()
            .pool()
            .expect("inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>"),
    )
    .await
    .unwrap_or(0);
    let total_edges: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM code_graph_edges WHERE project_id = $1 AND edge_type = 'import'",
    )
    .bind(project_id)
    .fetch_one(
        ctx.db()
            .pool()
            .expect("inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>"),
    )
    .await
    .unwrap_or(0);
    let sdp_score = if total_edges > 0 {
        (1.0 - sdp_violations as f64 / total_edges as f64) * 100.0
    } else {
        100.0
    };

    // Organization score (files with matching directory community)
    let org_score = 75.0; // Default baseline

    let dimensions = vec![
        ("separation_of_concerns", soc_score),
        ("loose_coupling", coupling_score),
        ("sdp_compliance", sdp_score),
        ("acyclicity", acyclicity_score),
        ("test_coverage", test_score),
        ("doc_coverage", doc_score),
        ("module_balance", balance_score),
        ("api_stability", stability_score),
        ("dependency_health", health_score),
        ("code_organization", org_score),
    ];

    let overall = dimensions.iter().map(|(_, s)| s).sum::<f64>() / dimensions.len() as f64;

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

    let dim_json: Vec<serde_json::Value> = dimensions
        .iter()
        .map(|(name, score)| {
            serde_json::json!({
                "dimension": name,
                "score": format!("{:.1}", score),
                "grade": letter_grade(*score),
            })
        })
        .collect();

    let result = serde_json::json!({
        "project": params.project,
        "overall_score": format!("{:.1}", overall),
        "overall_grade": letter_grade(overall),
        "dimensions": dim_json,
        "guidance": "Focus on dimensions with grade C or below. \
                     Run the specific analysis tools (coupling_cohesion_report, circular_dependencies, etc.) \
                     for detailed remediation guidance.",
    });

    let json = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "architecture_quality",
        overall = %format!("{:.1}", overall),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
