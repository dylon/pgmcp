//! `tool_read_file` — MCP tool body, extracted from `super::super::server`.
//!
//! Supports three modes:
//!
//! - **Full file** (no range params): return `indexed_files.content` as
//!   before. When `content` is NULL (Level-1 oversized file with no
//!   inline content), falls back to stitching from `file_chunks`.
//! - **Line region** (`start_line` + `end_line`): return only the chunks
//!   whose ranges overlap, trimmed to the requested lines.
//! - **Chunk range** (`chunk_index_start` + `chunk_index_end`): return
//!   the chunks in that index span verbatim.
//!
//! The region modes always also include `file_info`-style metadata so
//! callers can plan further reads without an extra round-trip.

#![allow(unused_imports)]

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, LoggingLevel};
use serde::Serialize;
use serde_json::json;
use tracing::{debug, error, info, warn};
use xxhash_rust::xxh3::xxh3_64;

use crate::context::SystemContext;
use crate::db::queries::{FileChunkRow, FileContent};
use crate::mcp::server::*;

#[derive(Debug, Serialize)]
struct ChunkResponse {
    chunk_index: i32,
    start_line: i32,
    end_line: i32,
    content: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
struct RegionResponse {
    path: String,
    language: Option<String>,
    /// True when the response is a strict subset of the file's chunks.
    is_region: bool,
    /// Total chunks in the file (so callers can page).
    total_chunks: i32,
    /// Effective line range covered by the returned chunks (1-based,
    /// inclusive). `None` for empty results.
    region_start_line: Option<i32>,
    region_end_line: Option<i32>,
    chunks: Vec<ChunkResponse>,
}

pub async fn tool_read_file(
    ctx: &SystemContext,
    params: ReadFileParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    debug!(
        tool = "read_file",
        path = %params.path,
        start_line = ?params.start_line,
        end_line = ?params.end_line,
        chunk_index_start = ?params.chunk_index_start,
        chunk_index_end = ?params.chunk_index_end,
        "MCP tool invoked"
    );

    let has_line_range = params.start_line.is_some() || params.end_line.is_some();
    let has_chunk_range = params.chunk_index_start.is_some() || params.chunk_index_end.is_some();
    if has_line_range && has_chunk_range {
        return Err(McpError::invalid_params(
            "Specify either start_line/end_line OR chunk_index_start/chunk_index_end, not both"
                .to_string(),
            None,
        ));
    }

    if has_line_range {
        return read_line_region(ctx, &params, start).await;
    }
    if has_chunk_range {
        return read_chunk_range(ctx, &params, start).await;
    }

    // Default: whole-file read. If `content` is NULL (Level-1 oversized
    // placeholder), fall back to stitching all chunks so the agent still
    // gets the indexed extracted text.
    let file = ctx.db().read_file(&params.path).await.map_err(|e| {
        error!(tool = "read_file", error = %e, "MCP tool failed");
        McpError::internal_error(format!("Read failed: {}", e), None)
    })?;

    let result = match file {
        None => {
            debug!(
                tool = "read_file",
                found = false,
                duration_ms = start.elapsed().as_millis() as u64,
                "MCP tool completed",
            );
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "File not found in index: {}",
                params.path
            ))]));
        }
        Some(mut file) if file.content.is_none() => {
            // Content is NULL: either the file is a Level-1 oversized
            // placeholder, or the indexer set `content_recoverable_from_disk
            // = true` for a plain-text language (asymmetric-storage policy
            // from PR D). Try the disk fast-path first; on hash mismatch,
            // disk-IO error, or `content_recoverable_from_disk = false`,
            // fall back to stitching all chunks.
            if file.content_recoverable_from_disk
                && let Some(expected_hash) = file.content_hash
            {
                match std::fs::read_to_string(&params.path) {
                    Ok(disk_bytes) => {
                        let disk_hash = xxh3_64(disk_bytes.as_bytes()) as i64;
                        if disk_hash == expected_hash {
                            ctx.stats()
                                .read_file_disk_hits
                                .fetch_add(1, Ordering::Relaxed);
                            file.content = Some(disk_bytes);
                            file
                        } else {
                            info!(
                                tool = "read_file",
                                path = %params.path,
                                "Disk file changed since indexing (hash mismatch); falling back to chunks"
                            );
                            ctx.stats()
                                .read_file_disk_hash_mismatches
                                .fetch_add(1, Ordering::Relaxed);
                            stitch_chunks(ctx, &params.path, file).await?
                        }
                    }
                    Err(e) => {
                        info!(
                            tool = "read_file",
                            path = %params.path,
                            error = %e,
                            "Disk read failed; falling back to chunks"
                        );
                        ctx.stats()
                            .read_file_disk_io_errors
                            .fetch_add(1, Ordering::Relaxed);
                        stitch_chunks(ctx, &params.path, file).await?
                    }
                }
            } else {
                stitch_chunks(ctx, &params.path, file).await?
            }
        }
        Some(file) => file,
    };

    let json = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "read_file",
        found = true,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

/// Recover `content` by fetching every chunk for the file and joining
/// them with `\n\n`. Falls back to leaving `content = None` if the file
/// has no chunks (e.g. Level-1 oversized placeholder with no extracted
/// text). Bumps `read_file_chunk_stitches` to record the slower path.
async fn stitch_chunks(
    ctx: &SystemContext,
    path: &str,
    mut file: FileContent,
) -> Result<FileContent, McpError> {
    ctx.stats()
        .read_file_chunk_stitches
        .fetch_add(1, Ordering::Relaxed);
    let summary = ctx
        .db()
        .file_chunk_summary(path)
        .await
        .map_err(|e| McpError::internal_error(format!("Chunk summary failed: {}", e), None))?;
    if summary.chunk_count > 0 {
        let chunks = ctx
            .db()
            .get_chunks_in_index_range(path, 0, i32::MAX)
            .await
            .map_err(|e| McpError::internal_error(format!("Chunk fetch failed: {}", e), None))?;
        let stitched = chunks
            .iter()
            .map(|c| c.content.as_str())
            .collect::<Vec<&str>>()
            .join("\n\n");
        file.content = Some(stitched);
    }
    Ok(file)
}

async fn read_line_region(
    ctx: &SystemContext,
    params: &ReadFileParams,
    start: Instant,
) -> Result<CallToolResult, McpError> {
    let start_line = params.start_line.unwrap_or(1).max(1);
    let end_line = params.end_line.unwrap_or(i32::MAX).max(start_line);

    let chunks = ctx
        .db()
        .get_file_region_by_lines(&params.path, start_line, end_line)
        .await
        .map_err(|e| McpError::internal_error(format!("Region fetch failed: {}", e), None))?;

    if chunks.is_empty() {
        return Ok(CallToolResult::success(vec![Content::text(format!(
            "No chunks overlap the requested line range [{}, {}] for path {}",
            start_line, end_line, params.path
        ))]));
    }

    finish_region(
        ctx,
        &params.path,
        clip_chunks_to_lines(chunks, start_line, end_line),
        start,
    )
    .await
}

async fn read_chunk_range(
    ctx: &SystemContext,
    params: &ReadFileParams,
    start: Instant,
) -> Result<CallToolResult, McpError> {
    let idx_start = params.chunk_index_start.unwrap_or(0).max(0);
    let idx_end = params.chunk_index_end.unwrap_or(i32::MAX).max(idx_start);

    let chunks = ctx
        .db()
        .get_chunks_in_index_range(&params.path, idx_start, idx_end)
        .await
        .map_err(|e| McpError::internal_error(format!("Chunk-range fetch failed: {}", e), None))?;

    if chunks.is_empty() {
        return Ok(CallToolResult::success(vec![Content::text(format!(
            "No chunks in range [{}, {}] for path {}",
            idx_start, idx_end, params.path
        ))]));
    }

    finish_region(ctx, &params.path, chunks, start).await
}

async fn finish_region(
    ctx: &SystemContext,
    path: &str,
    chunks: Vec<FileChunkRow>,
    start: Instant,
) -> Result<CallToolResult, McpError> {
    let summary = ctx
        .db()
        .file_chunk_summary(path)
        .await
        .map_err(|e| McpError::internal_error(format!("Chunk summary failed: {}", e), None))?;
    let info = ctx
        .db()
        .file_info(path)
        .await
        .map_err(|e| McpError::internal_error(format!("File info failed: {}", e), None))?;

    let region_start_line = chunks.first().map(|c| c.start_line);
    let region_end_line = chunks.last().map(|c| c.end_line);
    let chunk_responses: Vec<ChunkResponse> = chunks
        .into_iter()
        .map(|c| ChunkResponse {
            chunk_index: c.chunk_index,
            start_line: c.start_line,
            end_line: c.end_line,
            content: c.content,
        })
        .collect();

    let resp = RegionResponse {
        path: path.to_string(),
        language: info.map(|i| i.language),
        is_region: true,
        total_chunks: summary.chunk_count,
        region_start_line,
        region_end_line,
        chunks: chunk_responses,
    };

    let json = serde_json::to_string_pretty(&resp)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;
    debug!(
        tool = "read_file",
        is_region = true,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

fn clip_chunks_to_lines(
    chunks: Vec<FileChunkRow>,
    start_line: i32,
    end_line: i32,
) -> Vec<FileChunkRow> {
    chunks
        .into_iter()
        .map(|c| {
            let want_start = start_line.max(c.start_line);
            let want_end = end_line.min(c.end_line);
            if want_start == c.start_line && want_end == c.end_line {
                return c;
            }
            let body_lines: Vec<&str> = c.content.split('\n').collect();
            let total_in_chunk = (c.end_line - c.start_line) as usize + 1;
            let local_start = (want_start - c.start_line).max(0) as usize;
            let local_end = (want_end - c.start_line).min(total_in_chunk as i32 - 1) as usize;
            let clipped: Vec<&str> = body_lines
                .into_iter()
                .skip(local_start)
                .take(local_end - local_start + 1)
                .collect();
            FileChunkRow {
                chunk_index: c.chunk_index,
                start_line: want_start,
                end_line: want_end,
                content: clipped.join("\n"),
            }
        })
        .collect()
}
