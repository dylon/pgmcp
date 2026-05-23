//! `tool_grep` — MCP tool body, extracted from `super::super::server`.

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

use crate::context::SystemContext;
use crate::db::queries::GrepChunkResult;
use crate::mcp::server::*;

/// Per-match response envelope. Slimmer than `GrepChunkResult` for the
/// JSON wire — drops the raw chunk content when context lines are
/// requested so the agent only sees the narrow window around each match.
#[derive(Debug, Serialize)]
struct GrepHit {
    project_name: String,
    path: String,
    relative_path: String,
    language: String,
    chunk_index: i32,
    /// Matching window's first 1-based line number.
    window_start: i32,
    /// Matching window's last 1-based line number (inclusive).
    window_end: i32,
    /// Matching window's content (one or more lines from the chunk,
    /// bounded by `before_context` + match + `after_context`).
    content: String,
}

pub async fn tool_grep(
    ctx: &SystemContext,
    params: GrepParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().grep_searches.fetch_add(1, Ordering::Relaxed);

    let limit = params.limit.unwrap_or(10);
    let before = params.before_context.unwrap_or(0).max(0);
    let after = params.after_context.unwrap_or(0).max(0);
    let case_insensitive = params.case_insensitive.unwrap_or(false);

    debug!(
        tool = "grep",
        pattern = %truncate(&params.pattern, 200),
        glob = params.glob.as_deref().unwrap_or("*"),
        project = params.project.as_deref().unwrap_or("*"),
        language = params.language.as_deref().unwrap_or("*"),
        before_context = before,
        after_context = after,
        case_insensitive,
        limit,
        "MCP tool invoked",
    );

    let chunks = ctx
        .db()
        .grep_search_chunks(
            &params.pattern,
            params.project.as_deref(),
            params.language.as_deref(),
            params.glob.as_deref(),
            case_insensitive,
            limit,
            params.dedupe_worktrees.unwrap_or(false),
        )
        .await
        .map_err(|e| {
            error!(tool = "grep", error = %e, "MCP tool failed");
            McpError::internal_error(format!("Grep failed: {}", e), None)
        })?;

    let hits: Vec<GrepHit> = chunks
        .into_iter()
        .map(|m| build_hit(m, &params.pattern, before, after, case_insensitive))
        .collect();

    // Shadow-ASR Pattern D filter — restrict hits to enclosing-symbol facets.
    let hits = crate::mcp::tools::sema_helpers::filters::enclosing_symbol_filter_pass(
        ctx.db().pool(),
        hits,
        params.return_type_tags.as_deref(),
        params.effects.as_deref(),
        params.scope_kind.as_deref(),
    )
    .await;
    let count = hits.len();
    // Shadow-ASR channel (Phase D2b): workspace-wide effect distribution.
    let effect_breakdown: Vec<serde_json::Value> = (async {
        let Some(pool) = ctx.db().pool() else {
            return Vec::new();
        };
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT se.effect, COUNT(*)::int8
             FROM symbol_effects se
             GROUP BY se.effect
             ORDER BY se.effect",
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        rows.into_iter()
            .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
            .collect()
    })
    .await;

    let envelope = serde_json::json!({
        "hits": hits,
        "effect_breakdown": effect_breakdown,
    });
    let json = serde_json::to_string_pretty(&envelope)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "grep",
        results = count,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}

/// Given a matching chunk and the pattern, find the first match line
/// within the chunk and return the requested context window. When
/// `before == 0 && after == 0`, the entire chunk is returned (the
/// previous behavior was whole-file; this is still a ~10-100x token
/// reduction for typical document matches).
fn build_hit(
    chunk: GrepChunkResult,
    pattern: &str,
    before: i32,
    after: i32,
    case_insensitive: bool,
) -> GrepHit {
    if before == 0 && after == 0 {
        return GrepHit {
            project_name: chunk.project_name,
            path: chunk.path,
            relative_path: chunk.relative_path,
            language: chunk.language,
            chunk_index: chunk.chunk_index,
            window_start: chunk.start_line,
            window_end: chunk.end_line,
            content: chunk.content,
        };
    }

    // Find the first matching line within the chunk to anchor the
    // context window. We compile the regex defensively; if it fails to
    // compile we just return the full chunk content.
    let re = if case_insensitive {
        regex::RegexBuilder::new(pattern)
            .case_insensitive(true)
            .build()
    } else {
        regex::Regex::new(pattern)
    };
    let Ok(re) = re else {
        return GrepHit {
            project_name: chunk.project_name,
            path: chunk.path,
            relative_path: chunk.relative_path,
            language: chunk.language,
            chunk_index: chunk.chunk_index,
            window_start: chunk.start_line,
            window_end: chunk.end_line,
            content: chunk.content,
        };
    };

    let lines: Vec<&str> = chunk.content.split('\n').collect();
    let match_line_local = lines
        .iter()
        .position(|l| re.is_match(l))
        .map(|i| i as i32)
        .unwrap_or(0);

    let window_local_start = (match_line_local - before).max(0);
    let window_local_end = (match_line_local + after).min(lines.len() as i32 - 1);
    let window_slice: Vec<&&str> = lines
        .iter()
        .skip(window_local_start as usize)
        .take((window_local_end - window_local_start + 1) as usize)
        .collect();

    let window_start = chunk.start_line + window_local_start;
    let window_end = chunk.start_line + window_local_end;
    let content = window_slice
        .iter()
        .map(|s| **s)
        .collect::<Vec<&str>>()
        .join("\n");

    GrepHit {
        project_name: chunk.project_name,
        path: chunk.path,
        relative_path: chunk.relative_path,
        language: chunk.language,
        chunk_index: chunk.chunk_index,
        window_start,
        window_end,
        content,
    }
}
