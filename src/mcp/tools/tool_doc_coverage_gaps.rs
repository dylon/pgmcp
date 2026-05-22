//! `tool_doc_coverage_gaps` — MCP tool body, extracted from `super::super::server`.

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

pub async fn tool_doc_coverage_gaps(
    ctx: &SystemContext,
    params: DocCoverageGapsParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .doc_coverage_scans
        .fetch_add(1, Ordering::Relaxed);

    debug!(
        tool = "doc_coverage_gaps",
        project = %params.project,
        "MCP tool invoked",
    );

    let rows = ctx
        .db()
        .get_doc_topic_coverage(&params.project)
        .await
        .map_err(|e| McpError::internal_error(format!("Coverage query failed: {}", e), None))?;

    if rows.is_empty() {
        return Ok(CallToolResult::success(vec![Content::text(
            "No topic assignments found. Run discover_topics first.",
        )]));
    }

    let mut topics: Vec<serde_json::Value> = Vec::with_capacity(rows.len());
    let mut total_doc_chunks: i64 = 0;
    let mut total_code_chunks: i64 = 0;

    for row in &rows {
        total_doc_chunks += row.doc_chunks;
        total_code_chunks += row.code_chunks;

        let total = row.doc_chunks + row.code_chunks;
        let doc_ratio = if total > 0 {
            row.doc_chunks as f64 / total as f64
        } else {
            0.0
        };

        let status = if doc_ratio > 0.30 {
            "well-documented"
        } else if doc_ratio > 0.05 {
            "under-documented"
        } else {
            "undocumented"
        };

        topics.push(serde_json::json!({
            "topic_id": row.topic_id,
            "label": row.label,
            "keywords": row.keywords,
            "doc_chunks": row.doc_chunks,
            "code_chunks": row.code_chunks,
            "doc_ratio": format!("{:.2}", doc_ratio),
            "status": status,
        }));
    }

    // Sort by doc_ratio ascending (worst first)
    topics.sort_by(|a, b| {
        let ra: f64 = a["doc_ratio"]
            .as_str()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0.0);
        let rb: f64 = b["doc_ratio"]
            .as_str()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0.0);
        ra.partial_cmp(&rb).unwrap_or(std::cmp::Ordering::Equal)
    });

    let result = serde_json::json!({
        "project": params.project,
        "total_doc_chunks": total_doc_chunks,
        "total_code_chunks": total_code_chunks,
        "topic_count": topics.len(),
        "topics": topics,
        "guidance": "Topics marked 'undocumented' have code with no corresponding \
                     markdown documentation. Focus on topics with many code chunks \
                     but zero doc chunks. Consider creating documentation for these areas.",
    });

    let json = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "doc_coverage_gaps",
        topics = topics.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
