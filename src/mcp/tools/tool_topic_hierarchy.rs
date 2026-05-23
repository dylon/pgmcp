//! `tool_topic_hierarchy` — MCP tool body, extracted from `super::super::server`.

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

pub async fn tool_topic_hierarchy(
    ctx: &SystemContext,
    params: TopicHierarchyParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().hierarchy_scans.fetch_add(1, Ordering::Relaxed);

    let scope = params
        .project
        .as_deref()
        .map(|p| format!("project:{}", p))
        .unwrap_or_else(|| "global".to_string());

    debug!(
        tool = "topic_hierarchy",
        scope = %scope,
        num_groups = params.num_groups,
        "MCP tool invoked",
    );

    // If project specified but no cached topics, run a scan first
    if let Some(ref project_name) = params.project {
        let cached = ctx
            .db()
            .load_cached_topics(&scope, 1)
            .await
            .unwrap_or_default();

        if cached.is_empty() {
            let config = ctx.config().load();
            let min_cluster_size = config.cron.topic_min_cluster_size;
            crate::cron::topic_clustering::run_project_topic_scan(
                ctx.db().pool().expect(
                    "inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>",
                ),
                project_name,
                &config.cron,
                min_cluster_size,
                None,
            )
            .await
            .map_err(|e| McpError::internal_error(format!("Topic scan failed: {}", e), None))?;
        }
    }

    let centroids = ctx
        .db()
        .load_topic_centroids(&scope)
        .await
        .map_err(|e| McpError::internal_error(format!("Centroid query failed: {}", e), None))?;

    if centroids.len() < 2 {
        return Ok(CallToolResult::success(vec![Content::text(
            "Need at least 2 topics for hierarchy analysis. Run discover_topics first.",
        )]));
    }

    let num_groups = params
        .num_groups
        .map(|n| n as usize)
        .unwrap_or_else(|| (centroids.len() / 3).max(2));
    let num_groups = num_groups.min(centroids.len() - 1);

    let labels: Vec<String> = centroids.iter().map(|c| c.label.clone()).collect();
    let sizes: Vec<i64> = centroids.iter().map(|c| c.chunk_count).collect();
    let topic_ids: Vec<i32> = centroids.iter().map(|c| c.topic_id).collect();
    let vecs: Vec<&[f32]> = centroids.iter().map(|c| c.centroid.as_slice()).collect();

    let (groups, dendrogram) =
        agglomerative_cluster(&vecs, &labels, &sizes, &topic_ids, num_groups);

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
        "scope": scope,
        "topics_total": centroids.len(),
        "num_groups": groups.len(),
        "groups": groups,
        "dendrogram": dendrogram,
        "guidance": "Groups with low merge distance contain highly related topics that could \
                     be combined into a single module. The dendrogram shows the full merge history.",
    });

    let json = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "topic_hierarchy",
        groups = groups.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
