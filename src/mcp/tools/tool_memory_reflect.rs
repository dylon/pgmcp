//! Memory-server Phase 5: agent-driven `memory_reflect` MCP tool.
//!
//! Calls `llm::reflect::run_reflection` with `ReflectionTrigger::Agent`.
//! Refuses if the LLM extractor is disabled (`[memory.extractor]
//! backend = "disabled"`) or if `[memory.reflection] agent_enabled =
//! false` — silent no-op would mask a misconfig.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use uuid::Uuid;

use crate::context::SystemContext;
use crate::db::queries;
use crate::llm::reflect::{ReflectionRequest, ReflectionTrigger, run_reflection};
use crate::mcp::server::{MemoryReflectParams, MemoryScopeParam};

fn raw_pool(ctx: &SystemContext) -> Result<&sqlx::PgPool, McpError> {
    ctx.db()
        .pool()
        .ok_or_else(|| McpError::internal_error("raw pool unavailable", None))
}

fn parse_scope(p: Option<&MemoryScopeParam>) -> Result<queries::ScopeSpec, McpError> {
    let Some(p) = p else {
        return Ok(queries::ScopeSpec::default());
    };
    let session_id = match p.session_id.as_deref() {
        Some(s) => Some(Uuid::parse_str(s).map_err(|e| {
            McpError::invalid_params(format!("invalid session_id UUID: {}", e), None)
        })?),
        None => None,
    };
    Ok(queries::ScopeSpec {
        user_id: p.user_id.clone(),
        agent_id: p.agent_id.clone(),
        session_id,
        project_id: p.project_id,
    })
}

pub async fn tool_memory_reflect(
    ctx: &SystemContext,
    params: MemoryReflectParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "memory_reflect", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);

    // Refuse cleanly if the operator hasn't enabled agent-driven reflection.
    let cfg = ctx.config().load();
    if !cfg.memory.reflection.agent_enabled {
        return Err(McpError::invalid_params(
            "memory_reflect: agent_enabled = false in [memory.reflection]; enable it to use this tool",
            None,
        ));
    }

    let extractor = match ctx.llm_extractor() {
        Some(e) => e,
        None => {
            return Err(McpError::invalid_params(
                "memory_reflect: no LLM extractor available — set [memory.extractor] backend to a non-disabled value first",
                None,
            ));
        }
    };

    let pool = raw_pool(ctx)?;
    let stats = Arc::clone(ctx.stats_arc());

    // Optional scope; if provided, find_or_create_scope gives us a real id.
    let scope_id = if let Some(scope_param) = params.scope.as_ref() {
        let spec = parse_scope(Some(scope_param))?;
        Some(
            queries::find_or_create_scope(pool, &spec)
                .await
                .map_err(|e| McpError::internal_error(format!("scope: {}", e), None))?,
        )
    } else {
        None
    };

    let session_id = params
        .session_id
        .as_deref()
        .map(|s| {
            Uuid::parse_str(s)
                .map_err(|e| McpError::invalid_params(format!("invalid session_id: {}", e), None))
        })
        .transpose()?;

    let since = params
        .since
        .as_deref()
        .map(|s| {
            chrono::DateTime::parse_from_rfc3339(s)
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .map_err(|e| {
                    McpError::invalid_params(format!("invalid `since` RFC3339: {}", e), None)
                })
        })
        .transpose()?;

    let max_observations = params
        .max_observations
        .unwrap_or(cfg.memory.reflection.max_observations);

    let request = ReflectionRequest {
        scope_id,
        session_id,
        since,
        max_observations,
        trigger: ReflectionTrigger::Agent,
    };

    let report = run_reflection(pool, &stats, extractor.as_ref(), request)
        .await
        .map_err(|e| McpError::internal_error(format!("reflection failed: {}", e), None))?;

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
        "report": report,
        "effect_breakdown": effect_breakdown,
    });
    let text = serde_json::to_string_pretty(&envelope)
        .map_err(|e| McpError::internal_error(format!("serialize failed: {}", e), None))?;
    Ok(CallToolResult::success(vec![rmcp::model::Content::text(
        text,
    )]))
}
