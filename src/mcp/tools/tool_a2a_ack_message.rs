//! `tool_a2a_ack_message` — acknowledge a mailbox message for the caller's
//! session (stamps `acked_at`, which implies read + delivered).

#![allow(unused_imports)]

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::json;
use tracing::debug;

use crate::context::SystemContext;
use crate::mcp::server::*;

pub async fn tool_a2a_ack_message(
    ctx: &SystemContext,
    params: A2aAckMessageParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let Some(pool) = ctx.db().pool() else {
        return Err(McpError::internal_error(
            "database pool unavailable".to_string(),
            None,
        ));
    };

    let Some(sess) = params.session.as_deref() else {
        return Err(McpError::invalid_params(
            "session is required to acknowledge a message".to_string(),
            None,
        ));
    };

    crate::a2a::mailbox_store::record_receipt(
        pool,
        params.message_id,
        Some(sess),
        None,
        Some(crate::a2a::mailbox::DeliveryChannel::InboxPull.as_str()),
        crate::a2a::mailbox_store::Mark::Acked,
    )
    .await
    .map_err(|e| McpError::internal_error(format!("ack failed: {e}"), None))?;

    debug!(
        tool = "a2a_ack_message",
        message_id = params.message_id,
        "acked"
    );
    let body = serde_json::to_string_pretty(&json!({
        "acked": params.message_id,
        "by_session": sess,
    }))
    .map_err(|e| McpError::internal_error(format!("Serialization failed: {e}"), None))?;
    Ok(CallToolResult::success(vec![Content::text(body)]))
}
