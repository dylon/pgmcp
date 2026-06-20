//! New-corpus topic models (ADR-029, item 14): cluster work-items, commit
//! messages, and prompts into labeled themes via the shared
//! `crate::topic_apps::cluster_corpus` engine. Each tool fetches its embedded
//! corpus and returns themes (recurring concerns) — the topic model applied to
//! corpora beyond code chunks.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::server::{CommitTopicsParams, PromptTopicsParams, WorkItemTopicsParams};
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};
use crate::topic_apps::cluster_corpus;

/// Bound on items pulled into one clustering pass (FCM cost is ~O(n·k·d·iters)).
const MAX_CORPUS: i64 = 5000;

fn to_rows(recs: Vec<(i64, String, pgvector::Vector)>) -> Vec<(i64, String, Vec<f32>)> {
    recs.into_iter()
        .map(|(id, text, v)| (id, text, v.to_vec()))
        .collect()
}

fn cluster_result(
    corpus: &str,
    rows: Vec<(i64, String, Vec<f32>)>,
    k: usize,
) -> Result<CallToolResult, McpError> {
    let items = rows.len();
    let topics = cluster_corpus(&rows, k);
    json_result(&json!({
        "corpus": corpus,
        "items": items,
        "topic_count": topics.len(),
        "topics": topics,
        "guidance": if items == 0 {
            Some("no embedded items in this corpus yet — the embedding-migration cron populates the vectors")
        } else { None },
    }))
}

pub async fn tool_work_item_topics(
    ctx: &SystemContext,
    params: WorkItemTopicsParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let k = params.k.unwrap_or(8).clamp(1, 50) as usize;
    let recs: Vec<(i64, String, pgvector::Vector)> = match params.kind.as_deref() {
        Some(kind) => {
            sqlx::query_as(
                "SELECT id, COALESCE(title, '') , embedding FROM work_items
              WHERE embedding IS NOT NULL AND kind = $1 LIMIT $2",
            )
            .bind(kind)
            .bind(MAX_CORPUS)
            .fetch_all(pool)
            .await
        }
        None => {
            sqlx::query_as(
                "SELECT id, COALESCE(title, ''), embedding FROM work_items
              WHERE embedding IS NOT NULL LIMIT $1",
            )
            .bind(MAX_CORPUS)
            .fetch_all(pool)
            .await
        }
    }
    .map_err(|e| McpError::internal_error(format!("work_items: {e}"), None))?;
    cluster_result("work_items", to_rows(recs), k)
}

pub async fn tool_commit_topics(
    ctx: &SystemContext,
    params: CommitTopicsParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let k = params.k.unwrap_or(8).clamp(1, 50) as usize;
    let recs: Vec<(i64, String, pgvector::Vector)> = sqlx::query_as(
        "SELECT id, COALESCE(content, ''), embedding_v2 FROM git_commit_chunks
          WHERE embedding_v2 IS NOT NULL LIMIT $1",
    )
    .bind(MAX_CORPUS)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("git_commit_chunks: {e}"), None))?;
    cluster_result("git_commits", to_rows(recs), k)
}

pub async fn tool_prompt_topics(
    ctx: &SystemContext,
    params: PromptTopicsParams,
) -> Result<CallToolResult, McpError> {
    let pool = pool_or_err(ctx)?;
    let k = params.k.unwrap_or(8).clamp(1, 50) as usize;
    let recs: Vec<(i64, String, pgvector::Vector)> = sqlx::query_as(
        "SELECT id, COALESCE(prompt_text, ''), embedding_v2 FROM session_prompts
          WHERE embedding_v2 IS NOT NULL LIMIT $1",
    )
    .bind(MAX_CORPUS)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("session_prompts: {e}"), None))?;
    cluster_result("prompts", to_rows(recs), k)
}
