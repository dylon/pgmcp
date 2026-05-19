//! Memory-server Phase 8: explicit forget MCP tools.
//!
//! - `memory_forget` — soft- or cascade-delete a memory_* row by
//!   (target_type, target_id), always writes an audit-log entry.
//! - `memory_purge_expired` — admin-facing dry-run that reports which
//!   rows the `memory-retention` cron would delete with the current
//!   `window_days` and `importance_threshold`. `dry_run=false` actually
//!   performs the purge.

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::db::queries::{self, ForgetTargetType};
use crate::mcp::server::{MemoryForgetParams, MemoryPurgeExpiredParams};

fn raw_pool(ctx: &SystemContext) -> Result<&sqlx::PgPool, McpError> {
    ctx.db()
        .pool()
        .ok_or_else(|| McpError::internal_error("raw pool unavailable", None))
}

fn json_result(value: serde_json::Value) -> Result<CallToolResult, McpError> {
    let text = serde_json::to_string_pretty(&value)
        .map_err(|e| McpError::internal_error(format!("serialize failed: {}", e), None))?;
    Ok(CallToolResult::success(vec![rmcp::model::Content::text(
        text,
    )]))
}

pub async fn tool_memory_forget(
    ctx: &SystemContext,
    params: MemoryForgetParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    let target_type = ForgetTargetType::parse(&params.target_type)
        .map_err(|e| McpError::invalid_params(format!("{e}"), None))?;
    let cascade = params.cascade.unwrap_or(false);
    let actor = params.actor.unwrap_or_else(|| "agent".into());
    let report = queries::memory_forget(pool, target_type, params.target_id, cascade, &actor)
        .await
        .map_err(|e| McpError::internal_error(format!("memory_forget: {}", e), None))?;
    if cascade {
        ctx.stats()
            .memory_forget_cascade
            .fetch_add(1, Ordering::Relaxed);
    } else {
        ctx.stats()
            .memory_forget_soft
            .fetch_add(1, Ordering::Relaxed);
    }
    json_result(json!(report))
}

pub async fn tool_memory_purge_expired(
    ctx: &SystemContext,
    params: MemoryPurgeExpiredParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;
    let cfg = ctx.config().load();
    let window = params
        .window_days
        .unwrap_or(cfg.memory.retention.window_days);
    let imp = params
        .importance_threshold
        .unwrap_or(cfg.memory.retention.importance_threshold);
    let dry = params.dry_run.unwrap_or(true);
    if dry {
        let (e, o, r) = queries::memory_retention_dry_run(pool, window, imp)
            .await
            .map_err(|err| McpError::internal_error(format!("dry-run: {}", err), None))?;
        json_result(json!({
            "dry_run": true,
            "window_days": window,
            "importance_threshold": imp,
            "would_delete": {
                "entities": e,
                "observations": o,
                "relations": r,
            }
        }))
    } else {
        let (e, o, r) = queries::memory_retention_purge(pool, window, imp)
            .await
            .map_err(|err| McpError::internal_error(format!("purge: {}", err), None))?;
        ctx.stats()
            .memory_retention_entities_purged
            .fetch_add(e, Ordering::Relaxed);
        ctx.stats()
            .memory_retention_observations_purged
            .fetch_add(o, Ordering::Relaxed);
        ctx.stats()
            .memory_retention_relations_purged
            .fetch_add(r, Ordering::Relaxed);
        json_result(json!({
            "dry_run": false,
            "window_days": window,
            "importance_threshold": imp,
            "deleted": {
                "entities": e,
                "observations": o,
                "relations": r,
            }
        }))
    }
}
