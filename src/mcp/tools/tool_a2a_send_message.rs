//! `tool_a2a_send_message` — enqueue a message into a peer agent's mailbox,
//! addressable by session (precise instance, via `mcp_session_id`), project
//! (any agent working there), or agent type. Complements `a2a_send_task`
//! (spawn-RPC) with a mailbox to *live* instances.

#![allow(unused_imports)]

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::json;
use tracing::debug;

use crate::context::SystemContext;
use crate::mcp::server::*;
use crate::mcp::tools::sota_helpers::{pool_or_err, project_id_or_err};

pub async fn tool_a2a_send_message(
    ctx: &SystemContext,
    params: A2aSendMessageParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    if params.to_session.is_none() && params.to_project.is_none() && params.to_agent.is_none() {
        return Err(McpError::invalid_params(
            "specify at least one of to_session / to_project / to_agent".to_string(),
            None,
        ));
    }

    // The message body is required — trim it and reject blank.
    let body = params.body.trim();
    if body.is_empty() {
        return Err(McpError::invalid_params(
            "body must be non-empty".to_string(),
            None,
        ));
    }

    // Kind defaults to 'message'; trim and treat blank as the default, then
    // validate against the closed MessageKind vocabulary.
    let kind = params
        .kind
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("message");
    if crate::a2a::mailbox::MessageKind::parse(kind).is_none() {
        return Err(McpError::invalid_params(
            format!(
                "invalid kind '{kind}': expected message|request|fyi|request_worktree|accept|decline|moved"
            ),
            None,
        ));
    }

    // Optional short subject: trim and treat blank as absent.
    let subject = params
        .subject
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    // Resolve to_project name → id (must exist if given) — fail closed on
    // blank/unknown/duplicate via project_id_or_err.
    let to_project_id = match params.to_project.as_deref().map(str::trim) {
        Some(name) if !name.is_empty() => Some(project_id_or_err(ctx, name).await?),
        _ => None,
    };

    let from_agent = params.from_agent.as_deref().unwrap_or("unknown");
    let expires_at = params
        .expires_minutes
        .map(|m| chrono::Utc::now() + chrono::Duration::minutes(m));

    let msg = crate::a2a::mailbox_store::NewMessage {
        from_agent,
        from_session: params.from_session.as_deref(),
        to_session: params.to_session.as_deref(),
        to_project_id,
        to_agent: params.to_agent.as_deref(),
        kind,
        subject,
        body,
        reply_to: params.reply_to,
        expires_at,
    };
    let id = crate::a2a::mailbox_store::send(pool, &msg)
        .await
        .map_err(|e| McpError::internal_error(format!("send failed: {e}"), None))?;

    debug!(tool = "a2a_send_message", message_id = id, "sent");
    let body = serde_json::to_string_pretty(&json!({
        "message_id": id,
        "kind": kind,
        "to_session": params.to_session,
        "to_project_id": to_project_id,
        "to_agent": params.to_agent,
    }))
    .map_err(|e| McpError::internal_error(format!("Serialization failed: {e}"), None))?;
    Ok(CallToolResult::success(vec![Content::text(body)]))
}
