//! `tool_send_sync_violations` — Rust Send/Sync footgun detection (SOTA Phase 5.5).

#![allow(unused_imports)]

use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::SendSyncViolationsParams;
use crate::mcp::tools::sema_helpers::effects::symbols_with_effect;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};
use crate::mcp::tools::sota_regex_scan::scan_files_for_pattern;
use crate::parsing::type_tags::vocabulary::EFFECT_UNSAFE;

pub async fn tool_send_sync_violations(
    ctx: &SystemContext,
    params: SendSyncViolationsParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "send_sync_violations", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(50);

    let pat = Regex::new(
        r"(?m)Arc<RefCell\b|Arc<Cell\b|Rc<[^>]+>\s*(?:cloned|into).*spawn|static\s+mut\b|unsafe\s+impl\s+(Send|Sync)\b"
    ).expect("send/sync regex");
    let hits = scan_files_for_pattern(
        pool,
        project_id,
        &pat,
        Some(&["rust"]),
        limit.max(0) as usize,
    )
    .await
    .map_err(|e| McpError::internal_error(format!("Scan failed: {}", e), None))?;
    let rows: Vec<_> = hits
        .into_iter()
        .map(|h| json!({"file": h.relative_path, "line": h.line, "snippet": h.snippet}))
        .collect();
    // Shadow-ASR channel: symbols flagged with `unsafe` effect — Send/Sync
    // violations often co-locate with `unsafe impl Send/Sync` or `unsafe` blocks.
    let unsafe_symbols = symbols_with_effect(pool, project_id, EFFECT_UNSAFE)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(symbol_id, file_id, name, scope_path)| {
            serde_json::json!({
                "symbol_id": symbol_id, "file_id": file_id, "name": name, "scope_path": scope_path,
            })
        })
        .collect::<Vec<_>>();
    json_result(&json!({
        "project": params.project,
        "matches": rows,
        "unsafe_symbols": unsafe_symbols,
        "guidance": "Arc<RefCell<T>>/Arc<Cell<T>> across threads = data race; `static mut` = inherently unsynchronized global; `unsafe impl Send/Sync` requires manual audit of the invariants."
    }))
}
