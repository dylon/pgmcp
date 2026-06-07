//! `tool_lockset_races` — Heuristic for Eraser-style race candidates (SOTA Phase 5.1).
//!
//! Without intra-procedural lockset analysis, we surface call-sites where a
//! shared mutex/lock primitive is acquired and then released within the same
//! function — both candidates (lock fields without consistent guard scope)
//! and counterexamples (single-mutex, well-contained usage).

#![allow(unused_imports)]

use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::LocksetRacesParams;
use crate::mcp::tools::sema_helpers::effects::effect_counts;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};
use crate::mcp::tools::sota_regex_scan::scan_files_for_pattern;
use crate::parsing::type_tags::vocabulary::{TAG_ATOMIC, TAG_MUTEX};

const MAX_LOCKSET_RACE_LIMIT: i32 = 1_000;

pub async fn tool_lockset_races(
    ctx: &SystemContext,
    params: LocksetRacesParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "lockset_races", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project = params.project.trim();
    let project_id = project_id_or_err(ctx, project).await?;
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(50).clamp(1, MAX_LOCKSET_RACE_LIMIT);

    // Mutex/lock usage patterns across Rust, C++, Java, Go.
    let pat = Regex::new(
        r"(?m)\b(std::sync::Mutex|parking_lot::Mutex|tokio::sync::Mutex|RwLock|std::mutex|pthread_mutex_lock|synchronized\s*\(|Lock\.acquire|threading\.Lock|asyncio\.Lock|sync\.Mutex|sync\.RWMutex)\b"
    ).expect("lock pattern");
    let hits = scan_files_for_pattern(pool, project_id, &pat, None, limit as usize)
        .await
        .map_err(|e| McpError::internal_error(format!("Scan failed: {}", e), None))?;

    let rows: Vec<_> = hits
        .into_iter()
        .map(|h| json!({"file": h.relative_path, "language": h.language, "line": h.line, "snippet": h.snippet}))
        .collect();
    // Shadow-ASR channel: symbols whose parameters carry mutex / atomic
    // type tags. Read directly from `symbol_parameters.type_tags` array
    // overlap. Soft-fails to empty when the table is unpopulated.
    let mutex_typed_symbols: Vec<serde_json::Value> =
        sqlx::query_as::<_, (i64, i64, String, Option<String>)>(
            "SELECT DISTINCT fs.id, fs.file_id, fs.name, fs.scope_path
         FROM file_symbols fs
         JOIN indexed_files f ON f.id = fs.file_id
         JOIN symbol_parameters p ON p.symbol_id = fs.id
         WHERE f.project_id = $1
           AND p.type_tags && ARRAY[$2, $3]::text[]
         ORDER BY fs.file_id",
        )
        .bind(project_id)
        .bind(TAG_MUTEX)
        .bind(TAG_ATOMIC)
        .fetch_all(pool)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(symbol_id, file_id, name, scope_path)| {
            serde_json::json!({
                "symbol_id": symbol_id, "file_id": file_id, "name": name, "scope_path": scope_path,
            })
        })
        .collect();
    // Shadow-ASR channel (Phase D2b): project-scoped effect distribution.
    let mut effect_breakdown: Vec<(String, i64)> = effect_counts(pool, project_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .collect();
    effect_breakdown.sort_by(|a, b| a.0.cmp(&b.0));
    let effect_breakdown: Vec<serde_json::Value> = effect_breakdown
        .into_iter()
        .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
        .collect();

    json_result(&json!({
        "effect_breakdown": effect_breakdown,
        "project": project,
        "limit": limit,
        "matches": rows,
        "mutex_typed_symbols": mutex_typed_symbols,
        "guidance": "Surfaces concurrency primitives. To detect actual races (disjoint lock-sets across shared accesses) requires intra-procedural lockset analysis beyond regex; treat these as audit candidates and follow with manual review of variable scoping vs lock acquisition."
    }))
}
