//! `tool_design_metrics` — MCP tool body, extracted from `super::super::server`.

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

pub async fn tool_design_metrics(
    ctx: &SystemContext,
    params: DesignMetricsParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .design_metric_scans
        .fetch_add(1, Ordering::Relaxed);

    let scope = params.scope.as_deref().unwrap_or("project");
    let limit = params.limit.unwrap_or(30);
    let sort_by = params.sort_by.as_deref().unwrap_or("system_complexity");

    info!(
        tool = "design_metrics",
        project = %params.project,
        scope,
        limit,
        sort_by,
        "MCP tool invoked",
    );

    #[derive(sqlx::FromRow)]
    #[allow(dead_code)]
    struct FileRow {
        file_id: i64,
        relative_path: String,
        language: String,
        line_count: i32,
        content: Option<String>,
        in_degree: Option<i32>,
        out_degree: Option<i32>,
    }

    let path_filter = params.path.as_deref().unwrap_or("");
    let query = if path_filter.is_empty() || scope == "project" {
        "SELECT f.id as file_id, f.relative_path, f.language, f.line_count, f.content,
                fm.in_degree, fm.out_degree
         FROM indexed_files f
         LEFT JOIN file_metrics fm ON fm.file_id = f.id
         JOIN projects p ON f.project_id = p.id
         WHERE p.name = $1 AND f.content IS NOT NULL"
            .to_string()
    } else if scope == "directory" {
        format!(
            "SELECT f.id as file_id, f.relative_path, f.language, f.line_count, f.content,
                    fm.in_degree, fm.out_degree
             FROM indexed_files f
             LEFT JOIN file_metrics fm ON fm.file_id = f.id
             JOIN projects p ON f.project_id = p.id
             WHERE p.name = $1 AND f.content IS NOT NULL
               AND f.relative_path LIKE '{}%'",
            path_filter.replace('\'', "''")
        )
    } else {
        format!(
            "SELECT f.id as file_id, f.relative_path, f.language, f.line_count, f.content,
                    fm.in_degree, fm.out_degree
             FROM indexed_files f
             LEFT JOIN file_metrics fm ON fm.file_id = f.id
             JOIN projects p ON f.project_id = p.id
             WHERE p.name = $1 AND f.content IS NOT NULL
               AND f.relative_path = '{}'",
            path_filter.replace('\'', "''")
        )
    };

    let rows: Vec<FileRow> =
        sqlx::query_as::<_, FileRow>(&query)
            .bind(&params.project)
            .fetch_all(ctx.db().pool().expect(
                "inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>",
            ))
            .await
            .map_err(|e| McpError::internal_error(format!("Query failed: {}", e), None))?;

    if rows.is_empty() {
        return Ok(CallToolResult::success(vec![Content::text(
            "No files found matching the criteria.",
        )]));
    }

    // Compute metrics per file
    let branch_re = regex::Regex::new(
        r"(?m)^\s*(if|else\s+if|elif|else|for|while|match|case|catch|except|&&|\|\|)\b",
    )
    .expect("valid regex");

    let mut metrics: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let content = r.content.as_deref().unwrap_or("");

            // Cyclomatic complexity: count branching keywords + 1
            let branches = branch_re.find_iter(content).count();
            let cyclomatic = branches as i32 + 1;

            // WMC: cyclomatic per 100 lines (method density proxy)
            let wmc = if r.line_count > 0 {
                cyclomatic as f64 / (r.line_count as f64 / 100.0).max(1.0)
            } else {
                0.0
            };

            // Card & Glass structural complexity S(k) = fan_out^2
            let fan_out = r.out_degree.unwrap_or(0) as f64;
            let fan_in = r.in_degree.unwrap_or(0) as f64;
            let structural_complexity = fan_out * fan_out;
            // Data complexity D(k) approximated by fan_in * lines / fan_out
            let data_complexity = if fan_out > 0.0 {
                fan_in * r.line_count as f64 / (fan_out + 1.0)
            } else {
                0.0
            };
            // System complexity Sy(k) = S(k) + D(k)
            let system_complexity = structural_complexity + data_complexity;

            // Maintainability Index (simplified SEI formula)
            // MI = 171 - 5.2 * ln(HV) - 0.23 * CC - 16.2 * ln(LOC)
            // Using cyclomatic for CC, and lines for HV/LOC
            let loc = r.line_count.max(1) as f64;
            let halstead_volume = loc * loc.log2().max(1.0); // simplified
            let mi =
                (171.0 - 5.2 * halstead_volume.ln() - 0.23 * cyclomatic as f64 - 16.2 * loc.ln())
                    .clamp(0.0, 171.0);
            let mi_normalized = mi / 171.0 * 100.0;

            serde_json::json!({
                "path": r.relative_path,
                "language": r.language,
                "line_count": r.line_count,
                "cyclomatic_complexity": cyclomatic,
                "wmc": format!("{:.2}", wmc),
                "structural_complexity": format!("{:.1}", structural_complexity),
                "data_complexity": format!("{:.1}", data_complexity),
                "system_complexity": format!("{:.1}", system_complexity),
                "maintainability_index": format!("{:.1}", mi_normalized),
                "fan_in": r.in_degree.unwrap_or(0),
                "fan_out": r.out_degree.unwrap_or(0),
            })
        })
        .collect();

    // Sort
    match sort_by {
        "cyclomatic" => metrics.sort_by(|a, b| {
            let sa = a["cyclomatic_complexity"].as_i64().unwrap_or(0);
            let sb = b["cyclomatic_complexity"].as_i64().unwrap_or(0);
            sb.cmp(&sa)
        }),
        "maintainability" => metrics.sort_by(|a, b| {
            let sa: f64 = a["maintainability_index"]
                .as_str()
                .unwrap_or("100")
                .parse()
                .unwrap_or(100.0);
            let sb: f64 = b["maintainability_index"]
                .as_str()
                .unwrap_or("100")
                .parse()
                .unwrap_or(100.0);
            sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
        }),
        "wmc" => metrics.sort_by(|a, b| {
            let sa: f64 = a["wmc"].as_str().unwrap_or("0").parse().unwrap_or(0.0);
            let sb: f64 = b["wmc"].as_str().unwrap_or("0").parse().unwrap_or(0.0);
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        }),
        _ => metrics.sort_by(|a, b| {
            let sa: f64 = a["system_complexity"]
                .as_str()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0.0);
            let sb: f64 = b["system_complexity"]
                .as_str()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0.0);
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        }),
    }
    metrics.truncate(limit as usize);

    let result = serde_json::json!({
        "project": params.project,
        "scope": scope,
        "path": params.path,
        "sort_by": sort_by,
        "file_count": metrics.len(),
        "files": metrics,
        "guidance": "Cyclomatic complexity > 20 = high risk. Maintainability index < 50 = difficult to maintain. \
                     High system complexity (S+D) files are structural bottlenecks. \
                     WMC > 50 per 100 lines suggests excessive branching density.",
    });

    let json = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "design_metrics",
        files = metrics.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
