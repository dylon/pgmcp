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
use tracing::{debug, error};

use liblevenshtein::phonetic::token_grep::TokenGrep;

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

    // Fuzzy mode: approximate matching over indexed chunks via TokenGrep.
    if params.fuzzy.unwrap_or(false) {
        return fuzzy_grep(ctx, &params, limit, before, after).await;
    }

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
    // Shadow-ASR channel (Phase D2b): project-scoped effect distribution.
    let effect_breakdown = match ctx.db().pool() {
        Some(pool) => {
            let pid = crate::mcp::tools::sema_helpers::effects::project_id_opt(
                pool,
                params.project.as_deref(),
            )
            .await;
            crate::mcp::tools::sema_helpers::effects::effect_breakdown_json(pool, pid).await
        }
        None => serde_json::json!({}),
    };

    let mut envelope = serde_json::json!({
        "hits": hits,
        "effect_breakdown": effect_breakdown,
    });
    crate::mcp::tools::result_shaping::shape_search_results(
        &mut envelope,
        params.snippet_length.map(|n| n.max(0) as usize),
        params.fields.as_deref(),
        crate::mcp::client_profile::current_render_ctx(),
    );
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

/// Approximate (fuzzy) grep over indexed `file_chunks` via liblevenshtein's
/// `TokenGrep`. Fuzzy matching can't use an exact SQL prefilter (a typo'd query
/// must still find the correct spelling), so this fetches a broad bounded
/// candidate set of chunks for the project/glob (match-any) and scans each
/// fuzzily, returning the lowest-distance match per chunk. Strongly recommend a
/// `project` to bound the corpus.
async fn fuzzy_grep(
    ctx: &SystemContext,
    params: &GrepParams,
    limit: i32,
    before: i32,
    after: i32,
) -> Result<CallToolResult, McpError> {
    let max_d = params.fuzzy_max_distance.unwrap_or(2) as u8;
    let grep = TokenGrep::new(&params.pattern, max_d)
        .map_err(|e| McpError::internal_error(format!("fuzzy grep query: {e:?}"), None))?;

    // Candidate cap: scan a broad bounded set (fuzzy has no exact prefilter).
    let candidate_cap = limit.max(1).saturating_mul(200).min(5000);
    let chunks = ctx
        .db()
        .grep_search_chunks(
            ".", // match-any: candidate chunks for the project/glob
            params.project.as_deref(),
            params.language.as_deref(),
            params.glob.as_deref(),
            true,
            candidate_cap,
            params.dedupe_worktrees.unwrap_or(false),
        )
        .await
        .map_err(|e| McpError::internal_error(format!("fuzzy grep fetch: {}", e), None))?;
    let candidates_scanned = chunks.len();

    let mut hits: Vec<(u8, serde_json::Value)> = Vec::new();
    for chunk in &chunks {
        if let Some(best) = grep
            .scan(&chunk.content)
            .into_iter()
            .min_by_key(|m| m.total_distance)
        {
            let (window_start, window_end, content) = fuzzy_window(
                &chunk.content,
                best.byte_range.0,
                chunk.start_line,
                before,
                after,
            );
            hits.push((
                best.total_distance,
                serde_json::json!({
                    "project_name": chunk.project_name,
                    "path": chunk.path,
                    "relative_path": chunk.relative_path,
                    "language": chunk.language,
                    "chunk_index": chunk.chunk_index,
                    "window_start": window_start,
                    "window_end": window_end,
                    "content": content,
                    "distance": best.total_distance,
                    "matched_text": best.matched_text,
                }),
            ));
        }
    }
    hits.sort_by_key(|(d, _)| *d);
    let hits: Vec<serde_json::Value> = hits
        .into_iter()
        .take(limit.max(0) as usize)
        .map(|(_, h)| h)
        .collect();

    let mut envelope = serde_json::json!({
        "hits": hits,
        "fuzzy": true,
        "max_distance": max_d,
        "candidates_scanned": candidates_scanned,
    });
    crate::mcp::tools::result_shaping::shape_search_results(
        &mut envelope,
        params.snippet_length.map(|n| n.max(0) as usize),
        params.fields.as_deref(),
        crate::mcp::client_profile::current_render_ctx(),
    );
    let json = serde_json::to_string_pretty(&envelope)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

/// Map a byte offset within a chunk's content to a 1-based absolute line window
/// `[start, end]` (using the chunk's `start_line`) plus the windowed text.
fn fuzzy_window(
    content: &str,
    match_byte: usize,
    chunk_start_line: i32,
    before: i32,
    after: i32,
) -> (i32, i32, String) {
    let clamped = match_byte.min(content.len());
    let line_local = content[..clamped].bytes().filter(|&b| b == b'\n').count() as i32;
    let lines: Vec<&str> = content.split('\n').collect();
    let win_start = (line_local - before).max(0);
    let win_end = (line_local + after).min(lines.len() as i32 - 1);
    let text = lines
        .iter()
        .skip(win_start as usize)
        .take((win_end - win_start + 1).max(0) as usize)
        .copied()
        .collect::<Vec<&str>>()
        .join("\n");
    (
        chunk_start_line + win_start,
        chunk_start_line + win_end,
        text,
    )
}
