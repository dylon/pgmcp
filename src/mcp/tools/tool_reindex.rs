//! `tool_reindex` — MCP tool body, extracted from `super::super::server`.

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

pub async fn tool_reindex(ctx: &SystemContext) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    info!(tool = "reindex", "MCP tool invoked");

    // Synchronous (non-task) reindex: clear index directly
    sqlx::query("DELETE FROM file_chunks")
        .execute(
            ctx.db().pool().expect(
                "inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>",
            ),
        )
        .await
        .map_err(|e| {
            error!(tool = "reindex", error = %e, "Failed to clear chunks");
            McpError::internal_error(format!("Failed to clear chunks: {}", e), None)
        })?;

    sqlx::query("DELETE FROM indexed_files")
        .execute(
            ctx.db().pool().expect(
                "inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>",
            ),
        )
        .await
        .map_err(|e| {
            error!(tool = "reindex", error = %e, "Failed to clear files");
            McpError::internal_error(format!("Failed to clear files: {}", e), None)
        })?;

    ctx.log_broadcaster().log(
        LoggingLevel::Info,
        "pgmcp::reindex",
        serde_json::json!({"message": "Index cleared via reindex tool"}),
    );

    debug!(
        tool = "reindex",
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(
        "Index cleared. Files will be re-indexed automatically by the background scanner.",
    )]))
}
