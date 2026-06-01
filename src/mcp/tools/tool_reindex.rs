//! `tool_reindex` — MCP tool body, extracted from `super::super::server`.
//!
//! Phase D2b shadow-ASR contract (Pattern J): this is an admin tool that
//! returns a `format!`-built status string. It does not carry a JSON
//! envelope and is fire-and-forget at the API surface. The
//! `effect_breakdown` channel is intentionally NOT included here.
//! Clients can call `tool_index_stats` immediately after a reindex
//! completes to see the post-reindex effect distribution; that tool
//! returns the same statistics inside a JSON envelope with
//! `effect_breakdown` populated.

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

/// Number of `file_chunks` rows deleted per inner batch. Each batch
/// commits independently so a daemon shutdown / cancellation mid-reindex
/// surfaces between batches rather than at the end of a multi-minute
/// `DELETE`. 10k keeps each batch under a second on commodity hardware
/// against the production schema (`(file_id, chunk_index)` index).
const REINDEX_DELETE_BATCH: i64 = 10_000;

pub async fn tool_reindex(
    ctx: &SystemContext,
    params: ReindexParams,
) -> Result<CallToolResult, McpError> {
    let language = params.language;
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    debug!(tool = "reindex", language = ?language, "MCP tool invoked");

    // Refuse to run while another reindex is in flight. Two concurrent
    // reindexes race the embed pool's FK invariants and double the
    // wall-clock cost for no benefit.
    let _guard = ctx.reindex_lock().try_lock().map_err(|_| {
        warn!(
            tool = "reindex",
            "rejected — another reindex is in progress"
        );
        McpError::internal_error(
            "Another reindex is already in progress; retry after it completes.".to_string(),
            None,
        )
    })?;

    let pool = ctx
        .db()
        .pool()
        .expect("inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>");

    // Targeted re-extraction of a single language (no global wipe): invalidate
    // only that language's index rows so the background scanner re-extracts them
    // while every other file's Level-1 size+mtime skip is preserved. The narrow
    // mechanism for re-applying an extractor change (e.g. the LaTeX
    // pandoc→in-process cutover) without a self-DoS full rescan.
    if let Some(lang) = language {
        if ctx.lifecycle().is_stopping() {
            return Err(McpError::internal_error(
                "Reindex cancelled (daemon stopping)",
                None,
            ));
        }
        let files = crate::db::queries::delete_files_by_language(pool, &lang)
            .await
            .map_err(|e| {
                error!(tool = "reindex", language = %lang, error = %e, "Failed to clear files by language");
                McpError::internal_error(format!("Failed to clear {lang} files: {e}"), None)
            })?;
        ctx.log_broadcaster().log(
            LoggingLevel::Info,
            "pgmcp::reindex",
            serde_json::json!({
                "message": "Language index cleared via reindex tool",
                "language": lang,
                "deleted_files": files,
            }),
        );
        debug!(
            tool = "reindex",
            language = %lang,
            deleted_files = files,
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );
        return Ok(CallToolResult::success(vec![Content::text(format!(
            "Cleared {files} `{lang}` files from the index. They will be re-extracted \
             by the background scanner (restart the daemon to force an immediate full scan)."
        ))]));
    }

    // Batched delete with a between-batch cancel check. PostgreSQL deletes
    // are atomic per statement, so we commit each batch's transaction
    // before checking shutdown. Using `ctid` instead of `id` lets the
    // planner pick a sequential scan even when the FK index would
    // otherwise force a row-at-a-time delete.
    let mut total_chunks: i64 = 0;
    loop {
        if ctx.lifecycle().is_stopping() {
            warn!(
                tool = "reindex",
                deleted_chunks = total_chunks,
                "reindex cancelled mid-DELETE: daemon shutting down"
            );
            return Err(McpError::internal_error(
                format!("Reindex cancelled after deleting {total_chunks} chunks (daemon stopping)"),
                None,
            ));
        }
        let res = sqlx::query(
            "DELETE FROM file_chunks WHERE ctid IN \
             (SELECT ctid FROM file_chunks LIMIT $1)",
        )
        .bind(REINDEX_DELETE_BATCH)
        .execute(pool)
        .await
        .map_err(|e| {
            error!(tool = "reindex", error = %e, "Failed to delete chunks batch");
            McpError::internal_error(format!("Failed to delete chunks: {}", e), None)
        })?;
        let affected = res.rows_affected() as i64;
        total_chunks += affected;
        if affected == 0 {
            break;
        }
    }

    if ctx.lifecycle().is_stopping() {
        return Err(McpError::internal_error(
            "Reindex cancelled before deleting files (daemon stopping)",
            None,
        ));
    }

    let files_res = sqlx::query("DELETE FROM indexed_files")
        .execute(pool)
        .await
        .map_err(|e| {
            error!(tool = "reindex", error = %e, "Failed to clear files");
            McpError::internal_error(format!("Failed to clear files: {}", e), None)
        })?;
    let total_files = files_res.rows_affected();

    ctx.log_broadcaster().log(
        LoggingLevel::Info,
        "pgmcp::reindex",
        serde_json::json!({
            "message": "Index cleared via reindex tool",
            "deleted_chunks": total_chunks,
            "deleted_files": total_files,
        }),
    );

    debug!(
        tool = "reindex",
        deleted_chunks = total_chunks,
        deleted_files = total_files,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(format!(
        "Index cleared ({total_chunks} chunks, {total_files} files). \
         Files will be re-indexed automatically by the background scanner."
    ))]))
}
