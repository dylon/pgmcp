//! `tool_code_summarize` — MCP tool body, extracted from `super::super::server`.

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

pub async fn tool_code_summarize(
    ctx: &SystemContext,
    params: CodeSummarizeParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().summarize_scans.fetch_add(1, Ordering::Relaxed);

    let scope = params.scope.as_deref().unwrap_or("project");
    let detail = params.detail.as_deref().unwrap_or("standard");

    info!(
        tool = "code_summarize",
        project = %params.project,
        scope,
        path = params.path.as_deref().unwrap_or("*"),
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

    // Get directory-level summary
    #[derive(sqlx::FromRow)]
    struct DirSummary {
        directory: String,
        file_count: i64,
        total_lines: i64,
        languages: String,
    }

    let path_filter = params.path.as_deref().unwrap_or("");
    let dir_where = if !path_filter.is_empty() && scope != "project" {
        format!(
            "AND f.relative_path LIKE '{}%'",
            path_filter.replace('\'', "''")
        )
    } else {
        String::new()
    };

    let query = format!(
        "SELECT
            COALESCE(
                CASE WHEN position('/' IN relative_path) > 0
                    THEN left(relative_path, position('/' IN relative_path) - 1)
                    ELSE ''
                END, ''
            ) as directory,
            COUNT(*) as file_count,
            SUM(line_count)::BIGINT as total_lines,
            STRING_AGG(DISTINCT language, ', ') as languages
         FROM indexed_files f
         WHERE f.project_id = $1 {}
         GROUP BY directory
         ORDER BY file_count DESC
         LIMIT 30",
        dir_where
    );

    let dirs: Vec<DirSummary> =
        sqlx::query_as::<_, DirSummary>(&query)
            .bind(project_id)
            .fetch_all(ctx.db().pool().expect(
                "inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>",
            ))
            .await
            .map_err(|e| McpError::internal_error(format!("Dir query failed: {}", e), None))?;

    // Get top files by PageRank
    #[derive(sqlx::FromRow)]
    struct TopFile {
        relative_path: String,
        language: String,
        line_count: i32,
        pagerank: Option<f64>,
    }

    let top_files: Vec<TopFile> = sqlx::query_as::<_, TopFile>(
        "SELECT f.relative_path, f.language, f.line_count, fm.pagerank
         FROM indexed_files f
         LEFT JOIN file_metrics fm ON fm.file_id = f.id
         WHERE f.project_id = $1
         ORDER BY fm.pagerank DESC NULLS LAST
         LIMIT 10",
    )
    .bind(project_id)
    .fetch_all(
        ctx.db()
            .pool()
            .expect("inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>"),
    )
    .await
    .unwrap_or_default();

    // Get topic summary
    #[derive(sqlx::FromRow)]
    struct TopicSummary {
        label: String,
        chunk_count: i32,
    }

    let topics: Vec<TopicSummary> = sqlx::query_as::<_, TopicSummary>(
        "SELECT label, chunk_count
         FROM code_topics
         WHERE scope LIKE $1
         ORDER BY chunk_count DESC
         LIMIT 15",
    )
    .bind(format!("%{}", params.project))
    .fetch_all(
        ctx.db()
            .pool()
            .expect("inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>"),
    )
    .await
    .unwrap_or_default();

    // Language breakdown
    #[derive(sqlx::FromRow)]
    struct LangCount {
        language: String,
        count: i64,
        total_lines: i64,
    }

    let lang_breakdown: Vec<LangCount> = sqlx::query_as::<_, LangCount>(
        "SELECT language, COUNT(*) as count, SUM(line_count)::BIGINT as total_lines
         FROM indexed_files
         WHERE project_id = $1
         GROUP BY language
         ORDER BY count DESC",
    )
    .bind(project_id)
    .fetch_all(
        ctx.db()
            .pool()
            .expect("inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>"),
    )
    .await
    .unwrap_or_default();

    let total_files: i64 = lang_breakdown.iter().map(|l| l.count).sum();
    let total_lines: i64 = lang_breakdown.iter().map(|l| l.total_lines).sum();

    let dir_json: Vec<serde_json::Value> = dirs
        .iter()
        .map(|d| {
            serde_json::json!({
                "directory": if d.directory.is_empty() { "(root)" } else { &d.directory },
                "file_count": d.file_count,
                "total_lines": d.total_lines,
                "languages": d.languages,
            })
        })
        .collect();

    let key_files: Vec<serde_json::Value> = top_files
        .iter()
        .map(|f| {
            serde_json::json!({
                "path": f.relative_path,
                "language": f.language,
                "line_count": f.line_count,
                "pagerank": f.pagerank.map(|v| format!("{:.6}", v)),
            })
        })
        .collect();

    let topic_json: Vec<serde_json::Value> = topics
        .iter()
        .map(|t| {
            serde_json::json!({
                "topic": t.label,
                "chunk_count": t.chunk_count,
            })
        })
        .collect();

    let lang_json: Vec<serde_json::Value> = lang_breakdown
        .iter()
        .map(|l| {
            serde_json::json!({
                "language": l.language,
                "files": l.count,
                "lines": l.total_lines,
                "pct": format!("{:.1}%", l.count as f64 / total_files.max(1) as f64 * 100.0),
            })
        })
        .collect();

    let mut result = serde_json::json!({
        "project": params.project,
        "scope": scope,
        "total_files": total_files,
        "total_lines": total_lines,
        "language_breakdown": lang_json,
        "directories": dir_json,
        "key_files": key_files,
    });

    if detail != "brief"
        && let Some(o) = result.as_object_mut()
    {
        o.insert("topics".to_string(), serde_json::json!(topic_json));
    }

    let json = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "code_summarize",
        total_files,
        total_lines,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
