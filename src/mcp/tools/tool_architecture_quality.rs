//! `tool_architecture_quality` — MCP tool body.
//!
//! The 10-dimension scoring is factored into [`collect_architecture_dimensions`]
//! so both this tool and the `quality_report` aggregator share one
//! implementation. The thin tool wrapper keeps the stats counter, the
//! effect-breakdown channel, and the JSON envelope.

use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::json;
use tracing::debug;

use crate::context::SystemContext;
use crate::mcp::server::*;
use crate::mcp::tools::sota_helpers::{import_cycle_file_count, pool_or_err, project_id_or_err};
use crate::quality::report::{DimensionScore, letter_grade};

/// Compute the 10 architecture-quality dimensions for a project. Shared by the
/// `architecture_quality` tool and the `quality_report` Architecture pillar.
///
/// Each dimension is always scorable here (the underlying queries degrade to
/// neutral defaults), so all returned scores are `Some`. The `quality_report`
/// aggregator layers its data-absent supplementary dimensions on top.
pub(crate) async fn collect_architecture_dimensions(
    ctx: &SystemContext,
    project_id: i32,
) -> Result<Vec<DimensionScore>, McpError> {
    let pool = pool_or_err(ctx)?;

    // 1. Separation of concerns: avg distinct topics per file (lower = better).
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
    .fetch_optional(pool)
    .await
    .unwrap_or(None)
    .flatten();
    let soc_score = (1.0 - (avg_topics.unwrap_or(1.0) - 1.0).max(0.0) / 10.0).max(0.0) * 100.0;
    // Topic-derived: trustworthy only when the global topic model is fresh and
    // non-degenerate. When topics are absent or stale (e.g. computed by an older
    // tokenizer/label pipeline, the case that produced the `the/and/dylon`
    // stopword labels), this dimension is reported N/A and excluded from the
    // pillar mean — never scored a misleading 0.0.
    let topics_stale = crate::db::queries::topics_global_stale(
        pool,
        crate::cron::topic_clustering::TOPICS_ALGO_SIGNATURE,
    )
    .await;
    let soc_dim = if avg_topics.is_none() || topics_stale {
        DimensionScore::absent(
            "separation_of_concerns",
            "Avg distinct topics per file — N/A (topic model absent or stale)",
        )
    } else {
        DimensionScore::present(
            "separation_of_concerns",
            "Avg distinct topics per file (lower is better)",
            soc_score,
        )
    };

    // 2. Loose coupling: avg afferent+efferent coupling.
    let avg_coupling: Option<f64> = sqlx::query_scalar(
        "SELECT AVG(COALESCE(afferent_coupling, 0) + COALESCE(efferent_coupling, 0))::DOUBLE PRECISION
         FROM file_metrics fm
         JOIN indexed_files f ON f.id = fm.file_id AND f.project_id = fm.project_id
         WHERE fm.project_id = $1",
    )
    .bind(project_id)
    .fetch_optional(pool)
    .await
    .unwrap_or(None)
    .flatten();
    let coupling_score = (1.0 - avg_coupling.unwrap_or(0.0).min(20.0) / 20.0) * 100.0;

    // 3. Acyclicity: fraction of files NOT in import cycles.
    let total_files: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM indexed_files WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(pool)
            .await
            .unwrap_or(0);
    let files_in_cycles: i64 = import_cycle_file_count(pool, project_id).await.unwrap_or(0);
    let acyclicity_score = if total_files > 0 {
        (1.0 - files_in_cycles as f64 / total_files as f64) * 100.0
    } else {
        100.0
    };

    // 4. Test coverage: fraction of files that are tests.
    let test_file_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM indexed_files
         WHERE project_id = $1 AND relative_path ~* '(test|spec|_test\\.|_spec\\.)'",
    )
    .bind(project_id)
    .fetch_one(pool)
    .await
    .unwrap_or(0);
    let test_score = if total_files > 0 {
        (test_file_count as f64 / total_files as f64 * 3.0).min(1.0) * 100.0
    } else {
        0.0
    };

    // 5. Doc coverage: fraction of markdown files.
    let doc_file_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM indexed_files WHERE project_id = $1 AND language = 'markdown'",
    )
    .bind(project_id)
    .fetch_one(pool)
    .await
    .unwrap_or(0);
    let doc_score = if total_files > 0 {
        (doc_file_count as f64 / total_files as f64 * 10.0).min(1.0) * 100.0
    } else {
        0.0
    };

    // 6. Module balance: PageRank evenness.
    let avg_pagerank: Option<f64> = sqlx::query_scalar(
        "SELECT AVG(fm.pagerank)::DOUBLE PRECISION
         FROM file_metrics fm
         JOIN indexed_files f ON f.id = fm.file_id AND f.project_id = fm.project_id
         WHERE fm.project_id = $1",
    )
    .bind(project_id)
    .fetch_optional(pool)
    .await
    .unwrap_or(None)
    .flatten();
    let balance_score = avg_pagerank
        .map(|pr| {
            let expected = 1.0 / total_files.max(1) as f64;
            (1.0 - (pr - expected).abs() / expected.max(0.001)).max(0.0) * 100.0
        })
        .unwrap_or(50.0);

    // 7. API stability: inverse of churn.
    let avg_churn: Option<f64> = sqlx::query_scalar(
        "SELECT AVG(fm.churn_rate)::DOUBLE PRECISION
         FROM file_metrics fm
         JOIN indexed_files f ON f.id = fm.file_id AND f.project_id = fm.project_id
         WHERE fm.project_id = $1",
    )
    .bind(project_id)
    .fetch_optional(pool)
    .await
    .unwrap_or(None)
    .flatten();
    let stability_score = (1.0 - avg_churn.unwrap_or(0.0).min(5.0) / 5.0) * 100.0;

    // 8. Dependency health: inverse of fix-commit ratio.
    let avg_fix_ratio: Option<f64> = sqlx::query_scalar(
        "SELECT AVG(fm.fix_commit_ratio)::DOUBLE PRECISION
         FROM file_metrics fm
         JOIN indexed_files f ON f.id = fm.file_id AND f.project_id = fm.project_id
         WHERE fm.project_id = $1",
    )
    .bind(project_id)
    .fetch_optional(pool)
    .await
    .unwrap_or(None)
    .flatten();
    let health_score = (1.0 - avg_fix_ratio.unwrap_or(0.0)) * 100.0;

    // 9. SDP compliance: edges where stable doesn't depend on unstable.
    let sdp_violations: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM code_graph_edges e
         JOIN indexed_files f_s ON f_s.id = e.source_file_id AND f_s.project_id = e.project_id
         JOIN indexed_files f_t ON f_t.id = e.target_file_id AND f_t.project_id = e.project_id
         JOIN file_metrics fm_s ON fm_s.file_id = e.source_file_id AND fm_s.project_id = e.project_id
         JOIN file_metrics fm_t ON fm_t.file_id = e.target_file_id AND fm_t.project_id = e.project_id
         WHERE e.project_id = $1 AND e.edge_type = 'import'
           AND fm_s.instability < 0.3 AND fm_t.instability > 0.7",
    )
    .bind(project_id)
    .fetch_one(pool)
    .await
    .unwrap_or(0);
    let total_edges: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM code_graph_edges e
         JOIN indexed_files f_s ON f_s.id = e.source_file_id AND f_s.project_id = e.project_id
         JOIN indexed_files f_t ON f_t.id = e.target_file_id AND f_t.project_id = e.project_id
         WHERE e.project_id = $1 AND e.edge_type = 'import'",
    )
    .bind(project_id)
    .fetch_one(pool)
    .await
    .unwrap_or(0);
    let sdp_score = if total_edges > 0 {
        (1.0 - sdp_violations as f64 / total_edges as f64) * 100.0
    } else {
        100.0
    };

    // 10. Code organization — Card & Glass module-complexity surrogate.
    // (Replaces the former hardcoded 75.0 baseline: a real signal off the
    // already-loaded coupling average, normalized over a wider span than
    // `loose_coupling` so the two dims aren't identical.)
    let org_score = (1.0 - (avg_coupling.unwrap_or(0.0) / 30.0).clamp(0.0, 1.0)) * 100.0;

    Ok(vec![
        soc_dim,
        DimensionScore::present(
            "loose_coupling",
            "Avg afferent+efferent coupling",
            coupling_score,
        ),
        DimensionScore::present(
            "sdp_compliance",
            "Stable-dependencies-principle conformance",
            sdp_score,
        ),
        DimensionScore::present(
            "acyclicity",
            "Fraction of files free of import cycles",
            acyclicity_score,
        ),
        DimensionScore::present(
            "test_coverage",
            "Fraction of files that are tests",
            test_score,
        ),
        DimensionScore::present("doc_coverage", "Markdown documentation presence", doc_score),
        DimensionScore::present(
            "module_balance",
            "PageRank evenness across files",
            balance_score,
        ),
        DimensionScore::present("api_stability", "Inverse of change churn", stability_score),
        DimensionScore::present(
            "dependency_health",
            "Inverse of fix-commit ratio",
            health_score,
        ),
        DimensionScore::present(
            "code_organization",
            "Card & Glass module-complexity surrogate",
            org_score,
        ),
    ])
}

pub async fn tool_architecture_quality(
    ctx: &SystemContext,
    params: ArchitectureQualityParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .architecture_quality_scans
        .fetch_add(1, Ordering::Relaxed);

    let project = params.project.trim().to_string();
    let detail = params
        .detail
        .as_deref()
        .map(str::trim)
        .filter(|detail| !detail.is_empty())
        .unwrap_or("summary");
    if !matches!(detail, "summary" | "full") {
        return Err(McpError::invalid_params(
            format!(
                "Unknown detail '{}': expected one of summary | full",
                detail
            ),
            None,
        ));
    }
    debug!(
        tool = "architecture_quality",
        project = %project,
        detail,
        "MCP tool invoked",
    );

    let project_id = project_id_or_err(ctx, &project).await?;
    let dimensions = collect_architecture_dimensions(ctx, project_id).await?;

    // Average only the *scorable* dimensions; data-absent dims (e.g. a stale
    // topic model) are N/A and must not be counted as 0 in the denominator.
    let present: Vec<f64> = dimensions.iter().filter_map(|d| d.score).collect();
    let overall = if present.is_empty() {
        0.0
    } else {
        present.iter().sum::<f64>() / present.len() as f64
    };

    let dim_json: Vec<serde_json::Value> = dimensions
        .iter()
        .map(|d| {
            let mut value = match d.score {
                Some(score) => json!({
                    "dimension": d.name,
                    "score": format!("{:.1}", score),
                    "grade": letter_grade(score),
                }),
                None => json!({
                    "dimension": d.name,
                    "score": "N/A",
                    "grade": "N/A",
                }),
            };
            if detail == "full" {
                value["description"] = json!(d.description);
            }
            value
        })
        .collect();

    // Shadow-ASR channel: per-effect symbol-count breakdown for the project.
    let effect_breakdown: Vec<serde_json::Value> = (async {
        let Some(pool) = ctx.db().pool() else {
            return Vec::new();
        };
        crate::mcp::tools::sema_helpers::effects::effect_counts(pool, project_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(eff, count)| json!({ "effect": eff, "count": count }))
            .collect()
    })
    .await;

    let result = json!({
        "effect_breakdown": effect_breakdown,
        "project": project,
        "detail": detail,
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
