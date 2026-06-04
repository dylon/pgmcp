//! `tool_a2a_inbox` — read messages addressed to the caller (by session,
//! project, or agent type). The reliable pull floor of the mailbox; reading
//! marks the messages `read` (channel `inbox_pull`) for the caller's session.

#![allow(unused_imports)]

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::json;
use tracing::debug;

use crate::context::SystemContext;
use crate::mcp::server::*;

pub async fn tool_a2a_inbox(
    ctx: &SystemContext,
    params: A2aInboxParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let Some(pool) = ctx.db().pool() else {
        return Err(McpError::internal_error(
            "database pool unavailable".to_string(),
            None,
        ));
    };

    if params.session.is_none() && params.project.is_none() && params.agent.is_none() {
        return Err(McpError::invalid_params(
            "specify at least one of session / project / agent".to_string(),
            None,
        ));
    }

    // Resolve project name → id (a missing project just yields no project filter).
    let project_id: Option<i32> = match &params.project {
        Some(name) => sqlx::query_scalar("SELECT id FROM projects WHERE name = $1")
            .bind(name)
            .fetch_optional(pool)
            .await
            .map_err(|e| McpError::internal_error(format!("project lookup: {e}"), None))?,
        None => None,
    };

    let rows = crate::a2a::mailbox_store::inbox(
        pool,
        params.session.as_deref(),
        project_id,
        params.agent.as_deref(),
        params.unread_only,
    )
    .await
    .map_err(|e| McpError::internal_error(format!("inbox query failed: {e}"), None))?;

    // Reading marks the messages read for the caller's session (so the next-turn
    // / mid-loop delivery stages don't re-surface them).
    if let Some(sess) = params.session.as_deref() {
        for r in &rows {
            let _ = crate::a2a::mailbox_store::record_receipt(
                pool,
                r.id,
                Some(sess),
                params.agent.as_deref(),
                Some(crate::a2a::mailbox::DeliveryChannel::InboxPull.as_str()),
                crate::a2a::mailbox_store::Mark::Read,
            )
            .await;
        }
    }

    let messages: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.id,
                "from_agent": r.from_agent,
                "from_session": r.from_session,
                "kind": r.kind,
                "subject": r.subject,
                "body": r.body,
                "reply_to": r.reply_to,
                "created_at": r.created_at,
                "read_at": r.read_at,
                "acked_at": r.acked_at,
            })
        })
        .collect();
    let count = messages.len();
    debug!(tool = "a2a_inbox", count, "read inbox");
    let body = serde_json::to_string_pretty(&json!({ "count": count, "messages": messages }))
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {e}"), None))?;
    Ok(CallToolResult::success(vec![Content::text(body)]))
}
