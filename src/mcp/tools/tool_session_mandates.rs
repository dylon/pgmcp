//! MCP tools for session-scoped mandate introspection and promotion.

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use uuid::Uuid;

use crate::context::SystemContext;
use crate::mcp::server::*;
use crate::sessions;

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

fn parse_uuid(s: &str) -> Result<Uuid, McpError> {
    Uuid::parse_str(s)
        .map_err(|e| McpError::invalid_params(format!("invalid session_id UUID: {}", e), None))
}

pub async fn tool_session_mandates(
    ctx: &SystemContext,
    params: SessionMandatesParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;

    let session_id = params.session_id.as_deref().map(parse_uuid).transpose()?;
    let limit = params.limit.unwrap_or(20);
    let status_filter = params.status.as_deref().unwrap_or("active");

    if session_id.is_none() && params.cwd.is_none() {
        return Err(McpError::invalid_params(
            "either session_id or cwd is required",
            None,
        ));
    }

    let mut mandates = sessions::list_active_mandates(
        pool,
        session_id,
        params.cwd.as_deref(),
        limit.clamp(1, 100),
    )
    .await
    .map_err(|e| McpError::internal_error(format!("list_active_mandates failed: {}", e), None))?;

    // If status != active, the helper already filtered by status='active'.
    // For 'all' or non-default values, run a follow-up wider query.
    if status_filter != "active" {
        mandates = sqlx::query_as::<_, sessions::SessionMandate>(
            "SELECT * FROM session_mandates
             WHERE ($1::uuid IS NULL OR session_id = $1)
               AND ($2::text IS NULL OR status = $2)
             ORDER BY cue_tier DESC, last_reinforced_at DESC, salience DESC
             LIMIT $3",
        )
        .bind(session_id)
        .bind(if status_filter == "all" {
            None::<String>
        } else {
            Some(status_filter.to_string())
        })
        .bind(limit.clamp(1, 100))
        .fetch_all(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("query failed: {}", e), None))?;
    }

    let rendered = sessions::render_session_mandates_md(&mandates, 4096);

    json_result(json!({
        "session_id": session_id,
        "cwd": params.cwd,
        "count": mandates.len(),
        "mandates": mandates,
        "rendered_markdown": rendered,
    }))
}

pub async fn tool_promote_session_mandate(
    ctx: &SystemContext,
    params: PromoteSessionMandateParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = raw_pool(ctx)?;

    if !matches!(params.scope.as_str(), "project" | "workspace") {
        return Err(McpError::invalid_params(
            "scope must be 'project' or 'workspace'",
            None,
        ));
    }
    if params.scope == "project" && params.project_id.is_none() {
        return Err(McpError::invalid_params(
            "project_id is required when scope='project'",
            None,
        ));
    }

    let write_to_file = params.write_to_file.unwrap_or(false);
    let file_path_for_db: Option<String> = if write_to_file {
        params.target_file.clone()
    } else {
        None
    };

    let durable_id = sessions::promote_mandate(
        pool,
        params.mandate_id,
        &params.scope,
        params.project_id,
        file_path_for_db.as_deref(),
    )
    .await
    .map_err(|e| McpError::internal_error(format!("promote_mandate failed: {}", e), None))?;

    let mut written_path: Option<String> = None;
    if write_to_file {
        // Locate target file. v1 supports an explicit `target_file`; otherwise
        // refuse with a clear error rather than guess a path. File mutation is
        // gated under the explicit flag so callers can opt in deliberately.
        if let Some(path) = params.target_file.as_deref() {
            match append_mandate_to_file(path, params.mandate_id, &params.scope, pool).await {
                Ok(()) => written_path = Some(path.to_string()),
                Err(e) => {
                    return Err(McpError::internal_error(
                        format!("append_mandate_to_file({}): {}", path, e),
                        None,
                    ));
                }
            }
        } else {
            return Err(McpError::invalid_params(
                "write_to_file=true requires target_file (no implicit CLAUDE.md/AGENTS.md path is chosen for safety)",
                None,
            ));
        }
    }

    json_result(json!({
        "ok": true,
        "durable_mandate_id": durable_id,
        "source_session_mandate_id": params.mandate_id,
        "scope": params.scope,
        "wrote_file": written_path,
    }))
}

/// Append the imperative to a marker section in the target file. Idempotent:
/// if the section already contains the imperative, do nothing.
async fn append_mandate_to_file(
    path: &str,
    mandate_id: i64,
    scope: &str,
    pool: &sqlx::PgPool,
) -> Result<(), String> {
    const MARKER: &str = "## Promoted session mandates (pgmcp)";

    // Fetch the mandate text we're appending.
    let mandate = sessions::get_mandate(pool, mandate_id)
        .await
        .map_err(|e| format!("get_mandate: {}", e))?
        .ok_or_else(|| format!("mandate {} not found", mandate_id))?;
    let bullet = format!(
        "- **{}** _(scope: {})_: {}",
        mandate.polarity, scope, mandate.imperative
    );

    let existing = std::fs::read_to_string(path).unwrap_or_default();
    if existing.contains(&bullet) {
        return Ok(());
    }

    let mut out = existing.clone();
    if !out.contains(MARKER) {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
        out.push_str(MARKER);
        out.push_str("\n\n");
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&bullet);
    out.push('\n');

    std::fs::write(path, out).map_err(|e| format!("write {}: {}", path, e))?;
    Ok(())
}
