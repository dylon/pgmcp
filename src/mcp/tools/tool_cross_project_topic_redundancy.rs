//! `cross_project_topic_redundancy` — a new topic-model application (ADR-029,
//! item 14): surface GLOBAL topics whose chunks span multiple projects. Such
//! topics are shared concerns / fork-redundancy — strong consolidation
//! candidates. Pure read over the existing `code_topics` global model (no new
//! clustering): the topic model applied to cross-project SE intelligence.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::server::CrossProjectTopicRedundancyParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

pub async fn tool_cross_project_topic_redundancy(
    ctx: &SystemContext,
    params: CrossProjectTopicRedundancyParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let min_projects = params.min_projects.unwrap_or(2).max(2) as i32;
    let limit = params.limit.unwrap_or(50).clamp(1, 500);

    let rows = sqlx::query_as::<_, (String, i32, i32, i32, Vec<String>, Option<f64>)>(
        "SELECT label, project_count, chunk_count, file_count, project_names, avg_internal_similarity
           FROM code_topics
          WHERE scope = 'global' AND project_count >= $1
          ORDER BY project_count DESC, chunk_count DESC
          LIMIT $2",
    )
    .bind(min_projects)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("code_topics: {e}"), None))?;

    let topics: Vec<_> = rows
        .iter()
        .map(|(label, pc, cc, fc, projects, sim)| {
            json!({
                "label": label,
                "project_count": pc,
                "chunk_count": cc,
                "file_count": fc,
                "projects": projects,
                "cohesion": sim,
            })
        })
        .collect();

    json_result(&json!({
        "count": topics.len(),
        "min_projects": min_projects,
        "shared_topics": topics,
        "note": "Global topics whose chunks span ≥min_projects projects — shared concerns / \
    fork-redundancy / cross-project consolidation candidates (ranked by spread, then size).",
        "guidance": if topics.is_empty() {
            Some("no cross-project global topics — run trigger_cron job=\"topic-clustering\" to (re)build the global topic model")
        } else { None },
    }))
}
