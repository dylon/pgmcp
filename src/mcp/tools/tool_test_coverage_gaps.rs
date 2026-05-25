//! `tool_test_coverage_gaps` — MCP tool body, extracted from `super::super::server`.

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

pub async fn tool_test_coverage_gaps(
    ctx: &SystemContext,
    params: TestCoverageGapsParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().coverage_scans.fetch_add(1, Ordering::Relaxed);

    debug!(
        tool = "test_coverage_gaps",
        project = %params.project,
        "MCP tool invoked",
    );

    // Phase 4.4: REAL line coverage from any indexed lcov / Cobertura / JaCoCo
    // report, crossed with AST cyclomatic to surface high-complexity,
    // low-coverage files. Falls through to the topic proxy when no report
    // exists (opportunistic — gated on the repo actually shipping a report).
    let mut real_coverage = serde_json::Value::Null;
    if let Some(pool) = ctx.db().pool()
        && let Ok(Some(project_id)) =
            sqlx::query_scalar::<_, i32>("SELECT id FROM projects WHERE name = $1")
                .bind(&params.project)
                .fetch_optional(pool)
                .await
    {
        let artifacts: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT relative_path, content FROM indexed_files
             WHERE project_id = $1 AND content IS NOT NULL
               AND (relative_path LIKE '%lcov.info' OR relative_path LIKE '%.lcov'
                    OR relative_path LIKE '%coverage.xml' OR relative_path LIKE '%cobertura.xml'
                    OR relative_path LIKE '%jacoco.xml')",
        )
        .bind(project_id)
        .fetch_all(pool)
        .await
        .unwrap_or_default();

        let mut merged: Vec<crate::code_analysis::coverage::FileCoverage> = Vec::new();
        let mut formats: std::collections::BTreeSet<&'static str> =
            std::collections::BTreeSet::new();
        for (_p, content) in &artifacts {
            if let Some(c) = content
                && let Some((fmt, parsed)) = crate::code_analysis::coverage::detect_and_parse(c)
            {
                formats.insert(fmt.as_str());
                for fc in parsed {
                    if let Some(e) = merged.iter_mut().find(|m| m.path == fc.path) {
                        e.lines_total += fc.lines_total;
                        e.lines_covered += fc.lines_covered;
                    } else {
                        merged.push(fc);
                    }
                }
            }
        }
        if !merged.is_empty() {
            let tot: u32 = merged.iter().map(|m| m.lines_total).sum();
            let cov: u32 = merged.iter().map(|m| m.lines_covered).sum();
            let cc = crate::db::queries::get_file_ast_complexity_by_path(pool, project_id)
                .await
                .unwrap_or_default();
            let mut gaps: Vec<serde_json::Value> = Vec::new();
            for (path, max_cc, _, _) in &cc {
                if *max_cc < 8 {
                    continue; // only flag genuinely complex files
                }
                if let Some(m) = merged
                    .iter()
                    .find(|m| m.path.ends_with(path.as_str()) || path.ends_with(m.path.as_str()))
                {
                    let rate = if m.lines_total > 0 {
                        m.lines_covered as f64 / m.lines_total as f64
                    } else {
                        0.0
                    };
                    if rate < 0.5 {
                        gaps.push(json!({
                            "file": path, "max_cyclomatic": max_cc,
                            "line_rate": format!("{:.2}", rate),
                            "lines_total": m.lines_total, "lines_covered": m.lines_covered,
                        }));
                    }
                }
            }
            gaps.sort_by(|a, b| {
                b["max_cyclomatic"]
                    .as_i64()
                    .unwrap_or(0)
                    .cmp(&a["max_cyclomatic"].as_i64().unwrap_or(0))
            });
            gaps.truncate(40);
            real_coverage = json!({
                "formats": formats.into_iter().collect::<Vec<_>>(),
                "files_measured": merged.len(),
                "overall_line_rate": format!("{:.3}", if tot > 0 { cov as f64 / tot as f64 } else { 0.0 }),
                "lines_total": tot,
                "lines_covered": cov,
                "high_complexity_low_coverage": gaps,
            });
        }
    }

    let rows = ctx
        .db()
        .get_test_topic_coverage(&params.project)
        .await
        .map_err(|e| McpError::internal_error(format!("Coverage query failed: {}", e), None))?;

    if rows.is_empty() && real_coverage.is_null() {
        return Ok(CallToolResult::success(vec![Content::text(
            "No coverage report indexed and no topic assignments found. Index an lcov/Cobertura/\
             JaCoCo report, or run discover_topics for the topic-proxy view.",
        )]));
    }

    let mut topics: Vec<serde_json::Value> = Vec::with_capacity(rows.len());
    let mut total_test_chunks: i64 = 0;
    let mut total_impl_chunks: i64 = 0;

    for row in &rows {
        total_test_chunks += row.test_chunks;
        total_impl_chunks += row.impl_chunks;

        let total = row.test_chunks + row.impl_chunks;
        let test_ratio = if total > 0 {
            row.test_chunks as f64 / total as f64
        } else {
            0.0
        };

        let status = if test_ratio > 0.3 {
            "well-tested"
        } else if test_ratio > 0.01 {
            "under-tested"
        } else {
            "untested"
        };

        topics.push(serde_json::json!({
            "topic_id": row.topic_id,
            "label": row.label,
            "impl_chunks": row.impl_chunks,
            "test_chunks": row.test_chunks,
            "test_ratio": format!("{:.2}", test_ratio),
            "status": status,
        }));
    }

    // Sort by test ratio ascending (worst first)
    topics.sort_by(|a, b| {
        let ra: f64 = a["test_ratio"]
            .as_str()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0.0);
        let rb: f64 = b["test_ratio"]
            .as_str()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0.0);
        ra.partial_cmp(&rb).unwrap_or(std::cmp::Ordering::Equal)
    });

    // Shadow-ASR channel (Phase D2b): per-effect symbol-count breakdown
    // for the project. Universal enrichment — every tool benefits from
    // surfacing the effect distribution alongside its primary output.
    // Gracefully degrades to empty when the project lookup or
    // shadow-ASR data isn't populated.
    let effect_breakdown: Vec<serde_json::Value> = (async {
        let Some(pool) = ctx.db().pool() else {
            return Vec::new();
        };
        let project_id_opt: Option<i32> =
            sqlx::query_scalar("SELECT id FROM projects WHERE name = $1")
                .bind(&params.project)
                .fetch_optional(pool)
                .await
                .unwrap_or(None);
        match project_id_opt {
            Some(pid) => crate::mcp::tools::sema_helpers::effects::effect_counts(pool, pid)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
                .collect(),
            None => Vec::new(),
        }
    })
    .await;

    let result = serde_json::json!({
        "effect_breakdown": effect_breakdown,
        "project": params.project,
        "coverage_source": if real_coverage.is_null() { "topic_proxy" } else { "report+topic_proxy" },
        "real_coverage": real_coverage,
        "total_impl_chunks": total_impl_chunks,
        "total_test_chunks": total_test_chunks,
        "topic_count": topics.len(),
        "topics": topics,
        "guidance": "`real_coverage` (when present) is REAL line coverage parsed from an indexed lcov / \
                     Cobertura / JaCoCo report: `high_complexity_low_coverage` lists files with high AST \
                     cyclomatic AND <50% line coverage — the highest-leverage places to add tests. When no \
                     report is indexed it's null and only the topic proxy is shown: topics with 0% test \
                     coverage are the priority, especially those with many impl chunks but no test chunks.",
    });

    let json = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "test_coverage_gaps",
        topics = topics.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
