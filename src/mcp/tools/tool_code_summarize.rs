//! `tool_code_summarize` — MCP tool body, extracted from `super::super::server`.

#![allow(unused_imports)]

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, LoggingLevel};
use serde_json::json;
use tracing::debug;

use crate::context::SystemContext;
use crate::mcp::server::*;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_code_summarize(
    ctx: &SystemContext,
    params: CodeSummarizeParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().summarize_scans.fetch_add(1, Ordering::Relaxed);

    let project = params.project.trim();
    let scope = params
        .scope
        .as_deref()
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .unwrap_or("project");
    if !matches!(scope, "project" | "directory" | "file") {
        return Err(McpError::invalid_params(
            format!("unknown scope '{scope}'; expected project | directory | file"),
            None,
        ));
    }
    let detail = params
        .detail
        .as_deref()
        .map(str::trim)
        .filter(|detail| !detail.is_empty())
        .unwrap_or("standard");
    if !matches!(detail, "brief" | "standard" | "detailed") {
        return Err(McpError::invalid_params(
            format!("unknown detail '{detail}'; expected brief | standard | detailed"),
            None,
        ));
    }
    let path = params
        .path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty());
    if scope != "project" && path.is_none() {
        return Err(McpError::invalid_params(
            "path is required when scope is directory or file",
            None,
        ));
    }

    debug!(
        tool = "code_summarize",
        project,
        scope,
        path = path.unwrap_or("*"),
        detail,
        "MCP tool invoked",
    );

    let pool = pool_or_err(ctx)?;
    let project_id = project_id_or_err(ctx, project).await?;
    let directory_like = match (scope, path) {
        ("directory", Some(path)) => Some(format!("{}%", escape_like(path))),
        _ => None,
    };
    let file_exact = match (scope, path) {
        ("file", Some(path)) => Some(path),
        _ => None,
    };

    // Get directory-level summary
    #[derive(sqlx::FromRow)]
    struct DirSummary {
        directory: String,
        file_count: i64,
        total_lines: i64,
        languages: String,
    }

    let dirs: Vec<DirSummary> = sqlx::query_as::<_, DirSummary>(
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
         WHERE f.project_id = $1
           AND ($2::text IS NULL OR f.relative_path LIKE $2 ESCAPE '\\')
           AND ($3::text IS NULL OR f.relative_path = $3)
         GROUP BY directory
         ORDER BY file_count DESC
         LIMIT 30",
    )
    .bind(project_id)
    .bind(directory_like.as_deref())
    .bind(file_exact)
    .fetch_all(pool)
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
         LEFT JOIN file_metrics fm ON fm.file_id = f.id AND fm.project_id = f.project_id
         WHERE f.project_id = $1
           AND ($2::text IS NULL OR f.relative_path LIKE $2 ESCAPE '\\')
           AND ($3::text IS NULL OR f.relative_path = $3)
         ORDER BY fm.pagerank DESC NULLS LAST
         LIMIT 10",
    )
    .bind(project_id)
    .bind(directory_like.as_deref())
    .bind(file_exact)
    .fetch_all(pool)
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
         WHERE $1 = ANY(project_names)
         ORDER BY chunk_count DESC
         LIMIT 15",
    )
    .bind(project)
    .fetch_all(pool)
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
           AND ($2::text IS NULL OR relative_path LIKE $2 ESCAPE '\\')
           AND ($3::text IS NULL OR relative_path = $3)
         GROUP BY language
         ORDER BY count DESC",
    )
    .bind(project_id)
    .bind(directory_like.as_deref())
    .bind(file_exact)
    .fetch_all(pool)
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

    // Shadow-ASR channel (Phase D2b): per-effect symbol-count breakdown.
    let effect_breakdown: Vec<serde_json::Value> =
        crate::mcp::tools::sema_helpers::effects::effect_counts(pool, project_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
            .collect();

    let mut result = serde_json::json!({
        "project": project,
        "scope": scope,
        "path": path,
        "detail": detail,
        "total_files": total_files,
        "total_lines": total_lines,
        "language_breakdown": lang_json,
        "directories": dir_json,
        "key_files": key_files,
        "effect_breakdown": effect_breakdown,
    });

    if detail != "brief"
        && let Some(o) = result.as_object_mut()
    {
        o.insert("topics".to_string(), serde_json::json!(topic_json));
    }

    debug!(
        tool = "code_summarize",
        total_files,
        total_lines,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    json_result(&result)
}

fn escape_like(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\\' | '%' | '_' => {
                escaped.push('\\');
                escaped.push(ch);
            }
            _ => escaped.push(ch),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::escape_like;

    #[test]
    fn like_escape_treats_wildcards_literally() {
        assert_eq!(escape_like(r"src_%\generated"), r"src\_\%\\generated");
    }
}
