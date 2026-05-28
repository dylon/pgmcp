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
    debug!(tool = "orient", project = %params.project, "MCP tool invoked");

    let pool = ctx
        .db()
        .pool()
        .ok_or_else(|| McpError::internal_error("orient requires a real PgPool", None))?;

    let project_name = params.project.as_str();

    // Social-tool availability envelope (workspace-global). Rendered for both
    // the found and not-found paths so the nudge to use the A2A / CSM / memory /
    // work-item families reaches an agent even from an unindexed cwd. All counts
    // are best-effort — a missing table or query error yields 0, never fails.
    let social = social_envelope(pool).await;

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
            "social": social.clone(),
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
        keywords: Option<Vec<String>>,
        member_count: Option<i32>,
    }
    let topics: Vec<TopicSummary> = sqlx::query_as(
        "SELECT id AS topic_id, scope, keywords, chunk_count AS member_count
         FROM code_topics
         WHERE scope = $1 OR scope = 'global'
         ORDER BY chunk_count DESC NULLS LAST
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

    // Shadow-ASR channel (Phase D2b): workspace-wide effect distribution.
    let effect_breakdown: Vec<serde_json::Value> = (async {
        let Some(pool) = ctx.db().pool() else {
            return Vec::new();
        };
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT se.effect, COUNT(*)::int8
             FROM symbol_effects se
             GROUP BY se.effect
             ORDER BY se.effect",
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        rows.into_iter()
            .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
            .collect()
    })
    .await;

    let body = json!({
        "effect_breakdown": effect_breakdown,
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
            "keywords": t.keywords.as_ref(),
            "member_count": t.member_count,
        })).collect::<Vec<_>>(),
        "mandates": crate::mandates::compact_sources(&mandates),
        "social": social,
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

/// Workspace-global availability of the under-adopted tool families, with a
/// short `suggested_next` that adapts to whether peers/memory/work-items exist.
/// Surfaced in `orient` (which both Claude Code and Codex call first) so these
/// families get discovered through the one tool agents already reach for. Every
/// count is best-effort: a query error yields 0 so `orient` never fails on it.
async fn social_envelope(pool: &sqlx::PgPool) -> serde_json::Value {
    async fn count(pool: &sqlx::PgPool, sql: &str) -> i64 {
        let n: Result<i64, _> = sqlx::query_scalar(sql).fetch_one(pool).await;
        n.unwrap_or(0)
    }

    let a2a_peers = count(pool, "SELECT COUNT(*)::int8 FROM a2a_agents").await;
    let memory_entities = count(pool, "SELECT COUNT(*)::int8 FROM memory_entities").await;
    let memory_observations = count(pool, "SELECT COUNT(*)::int8 FROM memory_observations").await;
    let open_work_items = count(
        pool,
        "SELECT COUNT(*)::int8 FROM work_items \
         WHERE status NOT IN ('verified', 'cancelled', 'deferred')",
    )
    .await;

    let mut suggested_next: Vec<&str> = Vec::with_capacity(3);
    if a2a_peers == 0 {
        suggested_next.push(
            "No A2A peers registered — run `pgmcp a2a-adapter --kind claude --register-with \
             http://localhost:3100` (or enable [a2a] autostart_adapters) to unlock a2a_pattern_* \
             multi-agent collaboration.",
        );
    } else {
        suggested_next.push(
            "A2A peers available — a2a_find_agents_by_specialty + a2a_pattern_* for second \
             opinions / parallel specialists; csm_validate_run(task_id) after a pattern run.",
        );
    }
    if memory_observations == 0 {
        suggested_next.push(
            "Memory empty — memory_create_entities / memory_add_observations to persist durable \
             facts as you learn them.",
        );
    } else {
        suggested_next.push(
            "Memory populated — memory_unified_search to recall prior decisions before \
             re-deriving them.",
        );
    }
    if open_work_items > 0 {
        suggested_next.push(
            "Open work items exist — work_item_list / work_item_tree to review; \
             work_item_claim_next to pick one up.",
        );
    } else {
        suggested_next.push(
            "No open work items — work_item_create / work_item_ingest_plan to track multi-step \
             work spanning more than one session.",
        );
    }

    json!({
        "a2a_peers_registered": a2a_peers,
        "memory_entities": memory_entities,
        "memory_observations": memory_observations,
        "open_work_items": open_work_items,
        "suggested_next": suggested_next,
    })
}
