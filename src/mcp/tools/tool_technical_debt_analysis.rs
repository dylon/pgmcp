//! `tool_technical_debt_analysis` — MCP tool body, extracted from `super::super::server`.

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

const DEBT_MARKER_PATTERN: &str = r"(?i)\b(TODO|FIXME|HACK|XXX|TEMP|WORKAROUND)\b";

pub async fn tool_technical_debt_analysis(
    ctx: &SystemContext,
    params: TechnicalDebtAnalysisParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().debt_analyses.fetch_add(1, Ordering::Relaxed);

    let limit = params.limit.unwrap_or(30);
    let include_todos = params.include_todos.unwrap_or(true);

    debug!(
        tool = "technical_debt_analysis",
        project = %params.project,
        limit,
        include_todos,
        "MCP tool invoked",
    );

    #[derive(sqlx::FromRow)]
    #[allow(dead_code)]
    struct DebtRow {
        relative_path: String,
        language: String,
        line_count: i32,
        content: Option<String>,
        churn_rate: Option<f64>,
        fix_commit_ratio: Option<f64>,
        instability: Option<f64>,
    }

    let rows: Vec<DebtRow> = sqlx::query_as::<_, DebtRow>(
        "SELECT f.relative_path, f.language, f.line_count, f.content,
                fm.churn_rate, fm.fix_commit_ratio, fm.instability
         FROM indexed_files f
         LEFT JOIN file_metrics fm ON fm.file_id = f.id
         JOIN projects p ON f.project_id = p.id
         WHERE p.name = $1 AND f.content IS NOT NULL",
    )
    .bind(&params.project)
    .fetch_all(
        ctx.db()
            .pool()
            .expect("inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>"),
    )
    .await
    .map_err(|e| McpError::internal_error(format!("Query failed: {}", e), None))?;

    if rows.is_empty() {
        return Ok(CallToolResult::success(vec![Content::text(
            "No files found for this project.",
        )]));
    }

    let todo_re = regex::Regex::new(DEBT_MARKER_PATTERN).expect("valid regex");
    let branch_re =
        regex::Regex::new(r"(?m)^\s*(if|else\s+if|elif|for|while|match|case|catch|except)\b")
            .expect("valid regex");

    let mut total_todos = 0usize;
    let mut scored: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let content = r.content.as_deref().unwrap_or("");

            // Count debt markers
            let todo_count = if include_todos {
                todo_re.find_iter(content).count()
            } else {
                0
            };
            total_todos += todo_count;

            let todo_density = if r.line_count > 0 {
                todo_count as f64 / r.line_count as f64 * 1000.0
            } else {
                0.0
            };

            // Cyclomatic complexity
            let branches = branch_re.find_iter(content).count();
            let cyclomatic = branches as f64 + 1.0;
            let complexity_factor = (cyclomatic / 20.0).min(1.0);

            let churn = r.churn_rate.unwrap_or(0.0).min(10.0) / 10.0;
            let fix_ratio = r.fix_commit_ratio.unwrap_or(0.0);

            // Composite debt score
            let debt_score = todo_density * 0.3
                + complexity_factor * 0.25
                + churn * 0.2
                + fix_ratio * 0.15
                + (r.line_count as f64 / 1000.0).min(1.0) * 0.1;

            // Collect specific TODO lines
            let mut todo_lines: Vec<String> = Vec::new();
            if include_todos {
                for (i, line) in content.lines().enumerate() {
                    if todo_re.is_match(line) && todo_lines.len() < 5 {
                        todo_lines.push(format!("L{}: {}", i + 1, truncate(line.trim(), 120)));
                    }
                }
            }

            serde_json::json!({
                "path": r.relative_path,
                "language": r.language,
                "debt_score": format!("{:.4}", debt_score),
                "todo_count": todo_count,
                "todo_density": format!("{:.1}", todo_density),
                "cyclomatic_complexity": branches + 1,
                "line_count": r.line_count,
                "churn_rate": format!("{:.2}", r.churn_rate.unwrap_or(0.0)),
                "fix_ratio": format!("{:.2}", fix_ratio),
                "sample_todos": todo_lines,
            })
        })
        .collect();

    scored.sort_by(|a, b| {
        let sa: f64 = a["debt_score"]
            .as_str()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0.0);
        let sb: f64 = b["debt_score"]
            .as_str()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0.0);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.truncate(limit as usize);

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
        "total_debt_markers": total_todos,
        "file_count": scored.len(),
        "files": scored,
        "guidance": "Files with high debt_score combine TODO density, complexity, churn, and fix ratio. \
                     Address TODO/FIXME comments, reduce cyclomatic complexity, and stabilize high-churn files. \
                     todo_density is per 1000 lines.",
    });

    let json = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "technical_debt_analysis",
        files = scored.len(),
        total_todos,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}

#[cfg(test)]
mod tests {
    use super::DEBT_MARKER_PATTERN;

    #[test]
    fn debt_marker_regex_does_not_match_temp_substrings() {
        let re = regex::Regex::new(DEBT_MARKER_PATTERN).expect("valid regex");

        assert!(!re.is_match("let file = tempfile::NamedTempFile::new();"));
        assert!(!re.is_match("render_template(ctx)"));
        assert!(!re.is_match("temporary staging variable"));
    }

    #[test]
    fn debt_marker_regex_matches_explicit_markers() {
        let re = regex::Regex::new(DEBT_MARKER_PATTERN).expect("valid regex");

        assert!(re.is_match("// TODO: tighten this"));
        assert!(re.is_match("// FIXME handle error"));
        assert!(re.is_match("// TEMP cache until migration"));
        assert!(re.is_match("// WORKAROUND for upstream bug"));
    }
}
