//! `tool_orient` — composite "first-step" snapshot of a project.
//!
//! Bundles into a single MCP call what the model would otherwise spread
//! across `list_projects` + `project_tree` + `centrality_analysis` +
//! recently-changed-files queries. Designed to be the answer to
//! "I'm new to this codebase, where do I start?" and the recommended
//! first call when entering an unfamiliar workspace.
//!
//! Returns a JSON document with: project metadata, language breakdown,
//! depth-2 directory tree, top files by PageRank (key entry points),
//! recently-changed files (mtime), and a `health` envelope flagging
//! whether the index is mid-scan or whether graph metrics are stale.

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
use crate::db::queries::language_summary;
use crate::mcp::server::*;

pub async fn tool_orient(
    ctx: &SystemContext,
    params: OrientParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    info!(tool = "orient", project = %params.project, "MCP tool invoked");

    let pool = ctx
        .db()
        .pool()
        .ok_or_else(|| McpError::internal_error("orient requires a real PgPool", None))?;

    let project_name = params.project.as_str();

    // Project metadata
    let project_meta: Option<crate::db::queries::ProjectInfo> = sqlx::query_as(
        "SELECT p.id, p.workspace_path, p.path, p.name,
                p.git_common_dir, p.git_root_commits,
                p.discovered_at, p.last_scanned_at,
                (SELECT COUNT(*) FROM indexed_files f WHERE f.project_id = p.id) AS file_count
         FROM projects p
         WHERE p.name = $1
         LIMIT 1",
    )
    .bind(project_name)
    .fetch_optional(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Project lookup failed: {}", e), None))?;

    let Some(project) = project_meta else {
        let body = json!({
            "found": false,
            "project_name": project_name,
            "hint": "Project not found in pgmcp index. Use list_projects to see indexed projects.",
        });
        return Ok(CallToolResult::success(vec![Content::text(
            body.to_string(),
        )]));
    };

    // Languages
    let languages = language_summary(pool, project_name)
        .await
        .map_err(|e| McpError::internal_error(format!("language_summary failed: {}", e), None))?;

    // Depth-2 tree (capped to 200 entries to bound output)
    let tree: Vec<String> = ctx
        .db()
        .project_tree(project_name, 2)
        .await
        .map_err(|e| McpError::internal_error(format!("project_tree failed: {}", e), None))?
        .into_iter()
        .take(200)
        .collect();

    // Top 10 files by PageRank (key entry points). May be empty if the
    // graph-analysis cron hasn't run yet — callers see top_files=[] and
    // the `health.graph_stale` flag.
    #[derive(sqlx::FromRow)]
    struct EntryPoint {
        relative_path: String,
        language: String,
        pagerank: Option<f64>,
        in_degree: Option<i32>,
        out_degree: Option<i32>,
    }
    let entry_points: Vec<EntryPoint> = sqlx::query_as(
        "SELECT f.relative_path, f.language, fm.pagerank, fm.in_degree, fm.out_degree
         FROM file_metrics fm
         JOIN indexed_files f ON fm.file_id = f.id
         JOIN projects p ON fm.project_id = p.id
         WHERE p.name = $1 AND fm.pagerank IS NOT NULL
         ORDER BY fm.pagerank DESC NULLS LAST
         LIMIT 10",
    )
    .bind(project_name)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("entry_points failed: {}", e), None))?;

    // Recently-changed files (top 10 by indexed_at). This is a proxy
    // for "what's been touched recently" — the indexer updates
    // indexed_at when re-ingesting on file change.
    #[derive(sqlx::FromRow)]
    struct RecentFile {
        relative_path: String,
        language: String,
        indexed_at: Option<chrono::DateTime<chrono::Utc>>,
    }
    let recent: Vec<RecentFile> = sqlx::query_as(
        "SELECT f.relative_path, f.language, f.indexed_at
         FROM indexed_files f
         JOIN projects p ON f.project_id = p.id
         WHERE p.name = $1
         ORDER BY f.indexed_at DESC NULLS LAST
         LIMIT 10",
    )
    .bind(project_name)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("recent_files failed: {}", e), None))?;

    // Top topics (if discover_topics has been run for this project or
    // globally). Pulls top 8 by member count from code_topics, scoped
    // to project: prefix or global.
    #[derive(sqlx::FromRow)]
    struct TopicSummary {
        topic_id: i32,
        scope: String,
        keywords: Option<sqlx::types::Json<Vec<String>>>,
        member_count: Option<i32>,
    }
    let topics: Vec<TopicSummary> = sqlx::query_as(
        "SELECT topic_id, scope, keywords, member_count
         FROM code_topics
         WHERE scope = $1 OR scope = '*'
         ORDER BY member_count DESC NULLS LAST
         LIMIT 8",
    )
    .bind(format!("project:{}", project_name))
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("topics failed: {}", e), None))?;

    // Health envelope — flags that callers should respect. Lifecycle
    // phase tells the caller "index may be incomplete." Empty
    // entry_points + non-zero file_count suggests graph-analysis has
    // not yet run.
    let phase_label = ctx.lifecycle().current().label();
    let graph_stale = entry_points.is_empty() && project.file_count.unwrap_or(0) > 0;
    let topics_stale = topics.is_empty();
    let config = ctx.config().load();
    let mandates = crate::mandates::resolve_effective_mandates(&config, Some(&project));

    let body = json!({
        "found": true,
        "project_name": project.name,
        "project_root": project.path,
        "workspace_path": project.workspace_path,
        "file_count": project.file_count.unwrap_or(0),
        "discovered_at": project.discovered_at,
        "last_scanned_at": project.last_scanned_at,
        "languages": languages,
        "tree_depth_2": tree,
        "key_entry_points": entry_points.iter().map(|e| json!({
            "path": e.relative_path,
            "language": e.language,
            "pagerank": e.pagerank,
            "in_degree": e.in_degree,
            "out_degree": e.out_degree,
        })).collect::<Vec<_>>(),
        "recently_changed": recent.iter().map(|r| json!({
            "path": r.relative_path,
            "language": r.language,
            "indexed_at": r.indexed_at,
        })).collect::<Vec<_>>(),
        "top_topics": topics.iter().map(|t| json!({
            "topic_id": t.topic_id,
            "scope": t.scope,
            "keywords": t.keywords.as_ref().map(|k| &k.0),
            "member_count": t.member_count,
        })).collect::<Vec<_>>(),
        "mandates": crate::mandates::compact_sources(&mandates),
        "health": {
            "phase": phase_label,
            "graph_stale": graph_stale,
            "topics_stale": topics_stale,
            "guidance": if graph_stale || topics_stale {
                "Some derived data (graph metrics, topics) is missing or stale; \
                results from centrality_analysis/discover_topics will be limited \
                until the corresponding cron jobs have run."
            } else {
                "All derived data current."
            },
        },
    });

    debug!(
        tool = "orient",
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(
        body.to_string(),
    )]))
}
