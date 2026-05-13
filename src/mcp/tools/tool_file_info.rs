//! `tool_file_info` — MCP tool body, extracted from `super::super::server`.

#![allow(unused_imports)]

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use chrono::{DateTime, Utc};
use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, LoggingLevel};
use serde::Serialize;
use serde_json::json;
use tracing::{debug, error, info, warn};

use crate::context::SystemContext;
use crate::mcp::server::*;

/// Enriched file-info envelope. Adds chunk-aware metadata
/// (`chunk_count`, `first/last_chunk_line`) and a friendly
/// `extracted_kind` derived from `language` so clients can distinguish
/// "you're reading extracted PDF text" from "you're reading raw source".
#[derive(Debug, Serialize)]
struct EnrichedFileInfo {
    path: String,
    relative_path: String,
    language: String,
    size_bytes: i64,
    line_count: i32,
    truncated: bool,
    indexed_at: Option<DateTime<Utc>>,
    modified_at: DateTime<Utc>,
    chunk_count: i32,
    first_chunk_line: Option<i32>,
    last_chunk_line: Option<i32>,
    extracted_kind: Option<&'static str>,
}

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

    let Some(info) = info else {
        debug!(
            tool = "file_info",
            found = false,
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );
        return Ok(CallToolResult::success(vec![Content::text(format!(
            "File not found in index: {}",
            params.path
        ))]));
    };

    let summary = ctx
        .db()
        .file_chunk_summary(&params.path)
        .await
        .map_err(|e| {
            McpError::internal_error(format!("Chunk summary query failed: {}", e), None)
        })?;

    let enriched = EnrichedFileInfo {
        path: info.path,
        relative_path: info.relative_path,
        extracted_kind: extracted_kind_for(&info.language),
        language: info.language,
        size_bytes: info.size_bytes,
        line_count: info.line_count,
        truncated: info.truncated,
        indexed_at: info.indexed_at,
        modified_at: info.modified_at,
        chunk_count: summary.chunk_count,
        first_chunk_line: summary.first_chunk_line,
        last_chunk_line: summary.last_chunk_line,
    };

    let json = serde_json::to_string_pretty(&enriched)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;
    debug!(
        tool = "file_info",
        found = true,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

/// Map an indexed language to a human-readable "extracted_kind" label.
/// Lets clients distinguish content extracted from binary documents from
/// raw text. Returns `None` for languages that aren't routed through the
/// document extraction pipeline.
fn extracted_kind_for(language: &str) -> Option<&'static str> {
    match language {
        "pdf" => Some("pdf_text"),
        "postscript" => Some("postscript_text"),
        "docx" => Some("docx_text"),
        "doc" => Some("doc_text"),
        "rtf" => Some("rtf_text"),
        "odt" => Some("odt_text"),
        "epub" => Some("epub_text"),
        // text-source-but-pandoc-cleaned: the stored content is
        // post-pandoc plain text, not the raw markup.
        "latex" => Some("latex_plain"),
        "org" => Some("org_plain"),
        // Raw text passthroughs — caller is reading the file's bytes
        // as-is (with BOM stripped + Unicode normalized).
        "rst" => Some("rst_text"),
        "bibtex" => Some("bibtex_text"),
        "text" => Some("plain_text"),
        _ => None,
    }
}
