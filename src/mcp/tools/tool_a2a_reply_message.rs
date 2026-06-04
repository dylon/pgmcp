//! `tool_a2a_reply_message` — reply to a mailbox message; the reply is addressed
//! back to the original sender (its instance if known, else its agent type) and
//! linked via `reply_to`. Reading-to-reply also marks the original read.

#![allow(unused_imports)]

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::json;
use tracing::debug;

use crate::context::SystemContext;
use crate::mcp::server::*;

pub async fn tool_a2a_reply_message(
    ctx: &SystemContext,
    params: A2aReplyMessageParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let Some(pool) = ctx.db().pool() else {
        return Err(McpError::internal_error(
            "database pool unavailable".to_string(),
            None,
        ));
    };

    // Original sender → the reply's recipient.
    let orig: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT from_agent, from_session FROM agent_messages WHERE id = $1")
            .bind(params.message_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| McpError::internal_error(format!("lookup failed: {e}"), None))?;
    let Some((orig_from_agent, orig_from_session)) = orig else {
        return Err(McpError::invalid_params(
            format!("message {} not found", params.message_id),
            None,
        ));
    };

    let from_agent = params.from_agent.as_deref().unwrap_or("unknown");
    // Address the reply to the original sender's instance if we have it, else
    // broadcast back to its agent type.
    let to_agent = if orig_from_session.is_none() {
        Some(orig_from_agent.as_str())
    } else {
        None
    };
    let msg = crate::a2a::mailbox_store::NewMessage {
        from_agent,
        from_session: params.from_session.as_deref(),
        to_session: orig_from_session.as_deref(),
        to_project_id: None,
        to_agent,
        kind: "message",
        subject: None,
        body: &params.body,
        reply_to: Some(params.message_id),
        expires_at: None,
    };
    let reply_id = crate::a2a::mailbox_store::send(pool, &msg)
        .await
        .map_err(|e| McpError::internal_error(format!("reply send failed: {e}"), None))?;

    // Replying implies the replier read the original.
    if let Some(sess) = params.from_session.as_deref() {
        let _ = crate::a2a::mailbox_store::record_receipt(
            pool,
            params.message_id,
            Some(sess),
            params.from_agent.as_deref(),
            Some(crate::a2a::mailbox::DeliveryChannel::InboxPull.as_str()),
            crate::a2a::mailbox_store::Mark::Read,
        )
        .await;
    }

    debug!(
        tool = "a2a_reply_message",
        reply_id,
        in_reply_to = params.message_id,
        "replied"
    );
    let body = serde_json::to_string_pretty(&json!({
        "reply_id": reply_id,
        "in_reply_to": params.message_id,
        "to_session": orig_from_session,
        "to_agent": to_agent,
    }))
    .map_err(|e| McpError::internal_error(format!("Serialization failed: {e}"), None))?;
    Ok(CallToolResult::success(vec![Content::text(body)]))
}
