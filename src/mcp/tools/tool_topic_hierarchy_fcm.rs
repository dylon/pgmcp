//! `tool_topic_hierarchy_fcm` — MCP tool body, extracted from `super::super::server`.

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

pub async fn tool_topic_hierarchy_fcm(
    ctx: &SystemContext,
    params: TopicHierarchyFcmParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().hierarchy_scans.fetch_add(1, Ordering::Relaxed);

    let limit = params.limit.unwrap_or(50);

    debug!(tool = "topic_hierarchy_fcm", limit, "MCP tool invoked",);

    #[derive(sqlx::FromRow, serde::Serialize)]
    struct HierarchyRow {
        id: i64,
        cluster_index: i32,
        label: String,
        keywords: Option<Vec<String>>,
        parent_topic_ids: Option<Vec<i64>>,
    }

    let rows = sqlx::query_as::<_, HierarchyRow>(
        "SELECT id::bigint, cluster_index, label, keywords, parent_topic_ids
         FROM code_topics
         WHERE scope = 'hierarchy'
         ORDER BY cluster_index
         LIMIT $1",
    )
    .bind(limit as i64)
    .fetch_all(
        ctx.db()
            .pool()
            .expect("inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>"),
    )
    .await
    .map_err(|e| {
        error!(tool = "topic_hierarchy_fcm", error = %e, "MCP tool failed");
        McpError::internal_error(format!("Load hierarchy failed: {}", e), None)
    })?;

    let result = serde_json::json!({
        "scope": "hierarchy",
        "algorithm": "Fuzzy C-Means on global topic centroids",
        "meta_groups_found": rows.len(),
        "meta_groups": rows,
        "guidance": "Each meta_group.parent_topic_ids lists the global topic IDs \
                     composing that meta-group. Use discover_topics without a project \
                     param to get chunk-to-global-topic assignments, and this tool to \
                     navigate the higher-level semantic hierarchy. If no rows appear, \
                     run discover_topics with refresh=true first — hierarchy is chained \
                     after every global FCM run.",
    });

    let json = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "topic_hierarchy_fcm",
        meta_groups = rows.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
