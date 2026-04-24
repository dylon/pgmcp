//! `tool_file_info` — MCP tool body, extracted from `super::super::server`.

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

pub async fn tool_file_info(
    ctx: &SystemContext,
    params: FileInfoParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    info!(tool = "file_info", path = %params.path, "MCP tool invoked");

    let info = ctx.db().file_info(&params.path).await.map_err(|e| {
        error!(tool = "file_info", error = %e, "MCP tool failed");
        McpError::internal_error(format!("Query failed: {}", e), None)
    })?;

    let found = info.is_some();
    debug!(
        tool = "file_info",
        found,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    match info {
        Some(info) => {
            let json = serde_json::to_string_pretty(&info).map_err(|e| {
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
