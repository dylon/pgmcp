//! `tool_read_file` — MCP tool body, extracted from `super::super::server`.

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

pub async fn tool_read_file(
    ctx: &SystemContext,
    params: ReadFileParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    info!(tool = "read_file", path = %params.path, "MCP tool invoked");

    let result = ctx.db().read_file(&params.path).await.map_err(|e| {
        error!(tool = "read_file", error = %e, "MCP tool failed");
        McpError::internal_error(format!("Read failed: {}", e), None)
    })?;

    let found = result.is_some();
    debug!(
        tool = "read_file",
        found,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    match result {
        Some(file) => {
            let json = serde_json::to_string_pretty(&file).map_err(|e| {
                McpError::internal_error(format!("Serialization failed: {}", e), None)
            })?;
            Ok(CallToolResult::success(vec![Content::text(json)]))
        }
        None => Ok(CallToolResult::success(vec![Content::text(format!(
            "File not found in index: {}",
            params.path
        ))])),
    }
}
