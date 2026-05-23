//! `tool_bug_prediction` — MCP tool body, extracted from `super::super::server`.

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

pub async fn tool_bug_prediction(
    ctx: &SystemContext,
    params: BugPredictionParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().bug_predictions.fetch_add(1, Ordering::Relaxed);

    let limit = params.limit.unwrap_or(20);

    debug!(
        tool = "bug_prediction",
        project = %params.project,
        limit,
        "MCP tool invoked",
    );

    #[derive(sqlx::FromRow)]
    struct BugRow {
        relative_path: String,
        language: String,
        line_count: i32,
        churn_rate: Option<f64>,
        fix_commit_ratio: Option<f64>,
        commit_count: Option<i32>,
        author_count: Option<i32>,
        in_degree: Option<i32>,
        out_degree: Option<i32>,
    }

    let rows: Vec<BugRow> = sqlx::query_as::<_, BugRow>(
        "SELECT f.relative_path, f.language, f.line_count,
                fm.churn_rate, fm.fix_commit_ratio, fm.commit_count,
                fm.author_count, fm.in_degree, fm.out_degree
         FROM indexed_files f
         JOIN file_metrics fm ON fm.file_id = f.id
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

    if rows.is_empty() {
        return Ok(CallToolResult::success(vec![Content::text(
            "No file metrics found. The graph-analysis cron job may not have run yet.",
        )]));
    }

    // Compute complexity from branch keywords in content
    let mut scored: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let churn = r.churn_rate.unwrap_or(0.0);
            let fix_ratio = r.fix_commit_ratio.unwrap_or(0.0);
            let coupling = (r.in_degree.unwrap_or(0) + r.out_degree.unwrap_or(0)) as f64;
            let size_factor = (r.line_count as f64 / 100.0).min(10.0);
            let authors = r.author_count.unwrap_or(1) as f64;

            // Composite bug-proneness score
            // Weight: churn * fix_ratio * size * coupling * author_spread
            let bug_score = (churn * 0.3
                + fix_ratio * 3.0
                + size_factor * 0.2
                + coupling * 0.05
                + (authors - 1.0).max(0.0) * 0.1)
                .max(0.0);

            serde_json::json!({
                "path": r.relative_path,
                "language": r.language,
                "bug_score": format!("{:.4}", bug_score),
                "churn_rate": format!("{:.2}", churn),
                "fix_ratio": format!("{:.2}", fix_ratio),
                "line_count": r.line_count,
                "commit_count": r.commit_count.unwrap_or(0),
                "author_count": r.author_count.unwrap_or(0),
                "coupling": r.in_degree.unwrap_or(0) + r.out_degree.unwrap_or(0),
            })
        })
        .collect();

    scored.sort_by(|a, b| {
        let sa: f64 = a["bug_score"]
            .as_str()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0.0);
        let sb: f64 = b["bug_score"]
            .as_str()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0.0);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.truncate(limit as usize);

    // Shadow-ASR channel: bug-prone-effect symbols (unsafe / may_panic /
    // blocking_io). Composite bug-prediction can weigh these as features.
    let bug_prone_effect_symbols = if let Some(pool) = ctx.db().pool() {
        let project_id: Option<i32> = sqlx::query_scalar("SELECT id FROM projects WHERE name = $1")
            .bind(&params.project)
            .fetch_optional(pool)
            .await
            .unwrap_or(None);
        match project_id {
            Some(pid) => crate::mcp::tools::sema_helpers::effects::symbols_with_any_effect(
                pool,
                pid,
                &[
                    crate::parsing::type_tags::vocabulary::EFFECT_UNSAFE.to_string(),
                    crate::parsing::type_tags::vocabulary::EFFECT_MAY_PANIC.to_string(),
                    crate::parsing::type_tags::vocabulary::EFFECT_BLOCKING_IO.to_string(),
                ],
            )
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(symbol_id, file_id, name, scope_path)| {
                serde_json::json!({
                    "symbol_id": symbol_id, "file_id": file_id, "name": name, "scope_path": scope_path,
                })
            })
            .collect::<Vec<_>>(),
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };

    let result = serde_json::json!({
        "project": params.project,
        "file_count": scored.len(),
        "files": scored,
        "bug_prone_effect_symbols": bug_prone_effect_symbols,
        "guidance": "Files with high bug_score combine high churn, fix ratios, size, and coupling. \
                     Prioritize code review and testing for these files. \
                     High fix_ratio (>0.3) means >30% of commits are bug fixes. The `bug_prone_effect_symbols` channel surfaces symbols carrying unsafe / may_panic / blocking_io effects — orthogonal to the file-level metric and useful as additional review priorities.",
    });

    let json = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "bug_prediction",
        files = scored.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
