//! `tool_blocking_in_async` — Detect sync I/O / std::sync::Mutex / thread::sleep
//! inside `async fn` bodies (SOTA Phase 5.8).
//!
//! Phase D2b: returns two channels.
//! - `regex_matches`: legacy regex-on-content findings (preserved as the
//!   fallback when shadow-ASR data isn't yet populated).
//! - `effect_intersection`: symbols carrying BOTH `async` and
//!   `blocking_io` effects in the shadow-ASR catalog. Precise but
//!   limited to languages whose extractor emits those effects.

#![allow(unused_imports)]

use futures::TryStreamExt;
use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::BlockingInAsyncParams;
use crate::mcp::tools::sema_helpers::effects::symbols_with_all_effects;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};
use crate::parsing::type_tags::vocabulary::{EFFECT_ASYNC, EFFECT_BLOCKING_IO};

const MAX_BLOCKING_IN_ASYNC_LIMIT: i32 = 1_000;

pub async fn tool_blocking_in_async(
    ctx: &SystemContext,
    params: BlockingInAsyncParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "blocking_in_async", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project = params.project.trim();
    let project_id = project_id_or_err(ctx, project).await?;
    let pool = pool_or_err(ctx)?;
    let limit = params
        .limit
        .unwrap_or(50)
        .clamp(1, MAX_BLOCKING_IN_ASYNC_LIMIT);

    let mut rows = sqlx::query_as::<_, (String, String, Option<String>)>(
        "SELECT relative_path, language, content
         FROM indexed_files
         WHERE project_id = $1 AND content IS NOT NULL AND language IN ('rust','javascript','typescript','python')"
    )
    .bind(project_id)
    .fetch(pool);

    let async_fn_re =
        Regex::new(r"(?m)\b(async\s+fn|async\s+function|async\s+def)\b").expect("async fn regex");
    let blocking_re = Regex::new(
        r"(?m)\b(std::fs::|std::sync::Mutex::lock|std::thread::sleep|reqwest::blocking|fs\.readFileSync|fs\.writeFileSync|time\.sleep|requests\.get|requests\.post)\b"
    ).expect("blocking regex");

    let mut findings: Vec<serde_json::Value> = Vec::new();
    while let Some((path, lang, content)) = rows
        .try_next()
        .await
        .map_err(|e| McpError::internal_error(format!("Scan failed: {}", e), None))?
    {
        let Some(c) = content else { continue };
        // For each async fn body, count blocking calls.
        let mut anchors: Vec<usize> = async_fn_re.find_iter(&c).map(|m| m.end()).collect();
        anchors.push(c.len());
        for w in anchors.windows(2) {
            let start = w[0];
            // Walk until matching brace closes.
            let mut depth = 0i32;
            let mut seen_open = false;
            let mut end = w[1];
            for (i, ch) in c[start..].char_indices() {
                if ch == '{' {
                    depth += 1;
                    seen_open = true;
                } else if ch == '}' && seen_open {
                    depth -= 1;
                    if depth == 0 {
                        end = start + i;
                        break;
                    }
                }
            }
            let body = &c[start..end];
            for m in blocking_re.find_iter(body) {
                let line = c[..start + m.start()]
                    .bytes()
                    .filter(|b| *b == b'\n')
                    .count()
                    + 1;
                findings.push(json!({
                    "file": path,
                    "language": lang,
                    "line": line,
                    "blocking_call": m.as_str(),
                }));
                if findings.len() >= limit as usize {
                    drop(rows);
                    let effect_intersection = effect_channel(pool, project_id).await;
                    return done_with_effects(project, limit, findings, effect_intersection);
                }
            }
        }
    }
    drop(rows);
    let effect_intersection = effect_channel(pool, project_id).await;
    done_with_effects(project, limit, findings, effect_intersection)
}

fn done_with_effects(
    project: &str,
    limit: i32,
    regex_matches: Vec<serde_json::Value>,
    effect_intersection: Vec<serde_json::Value>,
) -> Result<CallToolResult, McpError> {
    json_result(&json!({
        "project": project,
        "limit": limit,
        "regex_matches": regex_matches,
        "effect_intersection": effect_intersection,
        "guidance": "Sync I/O / Mutex / sleep inside async functions blocks the runtime executor. Replace with tokio::fs, tokio::sync::Mutex, tokio::time::sleep, or a spawn_blocking shim. The `effect_intersection` channel surfaces symbols where the extractor flagged BOTH async and blocking_io effects (Rust backend only as of this writing)."
    }))
}

async fn effect_channel(pool: &sqlx::PgPool, project_id: i32) -> Vec<serde_json::Value> {
    // Symbols carrying BOTH `async` and `blocking_io` effects: the
    // structured complement to the regex pass. Currently only the Rust
    // backend emits both, but the API is uniform.
    let effects = vec![EFFECT_ASYNC.to_string(), EFFECT_BLOCKING_IO.to_string()];
    symbols_with_all_effects(pool, project_id, &effects)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(symbol_id, file_id, name, scope_path)| {
            json!({
                "symbol_id": symbol_id,
                "file_id": file_id,
                "name": name,
                "scope_path": scope_path,
            })
        })
        .collect()
}
