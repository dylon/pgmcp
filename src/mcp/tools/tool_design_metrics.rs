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

    debug!(
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

    // Rigorous AST metrics (from the function-metrics cron) keyed by file_id.
    // Files with parsed functions get AST-grade cyclomatic / WMC / Halstead /
    // Maintainability-Index; files with none fall back to the regex/line-count
    // heuristic below. Provenance is reported per file via the `source` field.
    let agg_map: std::collections::HashMap<i64, crate::db::queries::FileFunctionAggregate> =
        (async {
            let Some(pool) = ctx.db().pool() else {
                return std::collections::HashMap::new();
            };
            let pid: Option<i32> = sqlx::query_scalar("SELECT id FROM projects WHERE name = $1")
                .bind(&params.project)
                .fetch_optional(pool)
                .await
                .unwrap_or(None);
            match pid {
                Some(pid) => crate::db::queries::get_file_function_metric_aggregates(pool, pid)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|a| (a.file_id, a))
                    .collect(),
                None => std::collections::HashMap::new(),
            }
        })
        .await;

    // Compute metrics per file
    let branch_re = regex::Regex::new(
        r"(?m)^\s*(if|else\s+if|elif|else|for|while|match|case|catch|except|&&|\|\|)\b",
    )
    .expect("valid regex");

    let mut metrics: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let content = r.content.as_deref().unwrap_or("");

            // Card & Glass structural/data/system complexity from the real
            // import-graph degrees — already rigorous, source-independent.
            let fan_out = r.out_degree.unwrap_or(0) as f64;
            let fan_in = r.in_degree.unwrap_or(0) as f64;
            let structural_complexity = fan_out * fan_out;
            let data_complexity = if fan_out > 0.0 {
                fan_in * r.line_count as f64 / (fan_out + 1.0)
            } else {
                0.0
            };
            let system_complexity = structural_complexity + data_complexity;

            // Cyclomatic / WMC / Halstead / MI: prefer the rigorous AST track
            // (function_metrics) when the file has parsed functions; otherwise
            // fall back to the regex/line-count heuristic. `source` reports which.
            #[allow(clippy::type_complexity)]
            let (
                cyclomatic,
                wmc,
                halstead_volume,
                cognitive,
                mi_normalized,
                mi_min,
                function_count,
                source,
            ): (i32, f64, f64, i64, f64, f64, i64, &str) =
                match agg_map.get(&r.file_id).filter(|a| a.function_count > 0) {
                    Some(a) => (
                        a.max_cyclomatic,        // worst single function
                        a.sum_cyclomatic as f64, // true WMC = Σ method complexity
                        a.sum_halstead_volume,
                        a.sum_cognitive,
                        a.avg_maintainability, // function_metrics MI is already on 0..100
                        a.min_maintainability,
                        a.function_count,
                        "ast",
                    ),
                    None => {
                        let branches = branch_re.find_iter(content).count();
                        let cyclomatic = branches as i32 + 1;
                        let wmc = if r.line_count > 0 {
                            cyclomatic as f64 / (r.line_count as f64 / 100.0).max(1.0)
                        } else {
                            0.0
                        };
                        let loc = r.line_count.max(1) as f64;
                        let halstead_volume = loc * loc.log2().max(1.0); // simplified
                        let mi = (171.0
                            - 5.2 * halstead_volume.ln()
                            - 0.23 * cyclomatic as f64
                            - 16.2 * loc.ln())
                        .clamp(0.0, 171.0);
                        let mi_norm = mi / 171.0 * 100.0;
                        (
                            cyclomatic,
                            wmc,
                            halstead_volume,
                            0i64,
                            mi_norm,
                            mi_norm,
                            0i64,
                            "heuristic",
                        )
                    }
                };

            serde_json::json!({
                "path": r.relative_path,
                "language": r.language,
                "line_count": r.line_count,
                "source": source,
                "function_count": function_count,
                "cyclomatic_complexity": cyclomatic,
                "cognitive_complexity": cognitive,
                "wmc": format!("{:.2}", wmc),
                "halstead_volume": format!("{:.1}", halstead_volume),
                "structural_complexity": format!("{:.1}", structural_complexity),
                "data_complexity": format!("{:.1}", data_complexity),
                "system_complexity": format!("{:.1}", system_complexity),
                "maintainability_index": format!("{:.1}", mi_normalized),
                "maintainability_index_min": format!("{:.1}", mi_min),
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
        "scope": scope,
        "path": params.path,
        "sort_by": sort_by,
        "file_count": metrics.len(),
        "files": metrics,
        "guidance": "Each file reports `source`: \"ast\" = rigorous per-function metrics (real cyclomatic / cognitive / \
                     Halstead / Maintainability-Index from the function-metrics cron) aggregated to the file — there \
                     `cyclomatic_complexity` is the worst single function, `wmc` is the Chidamber-Kemerer Σ-of-method \
                     complexity, `maintainability_index` is the per-function average (`maintainability_index_min` the \
                     worst). \"heuristic\" = a regex branch-count fallback (file unparsed, or the cron hasn't run yet). \
                     Any function with cyclomatic > 20 is high risk; maintainability index < 50 = hard to maintain; \
                     high system complexity (S+D) marks structural bottlenecks.",
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
