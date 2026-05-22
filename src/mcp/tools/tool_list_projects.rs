//! `tool_list_projects` — MCP tool body, extracted from `super::super::server`.

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

pub async fn tool_list_projects(ctx: &SystemContext) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    debug!(tool = "list_projects", "MCP tool invoked");

    let projects = ctx.db().list_projects().await.map_err(|e| {
        error!(tool = "list_projects", error = %e, "MCP tool failed");
        McpError::internal_error(format!("Query failed: {}", e), None)
    })?;

    let count = projects.len();
    let json = serde_json::to_string_pretty(&projects)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "list_projects",
        results = count,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
