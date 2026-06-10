//! `tool_coordination_respond` — an editor on a dependency project responds to a
//! `request_worktree` coordination: accept | decline | moved. `moved` is a
//! CANDIDATE only — the dependent is unblocked solely when pgmcp's git scanner
//! observes the dependency stable (the gatekeeper trust boundary). The requester
//! is notified of the response via the mailbox.

#![allow(unused_imports)]

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::json;
use tracing::debug;

use crate::context::SystemContext;
use crate::deps::coordination::CoordinationStatus;
use crate::mcp::server::*;
use crate::mcp::tools::sota_helpers::{pool_or_err, project_id_or_err};

pub async fn tool_coordination_respond(
    ctx: &SystemContext,
    params: CoordinationRespondParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let status = match params.response.trim() {
        "accept" | "accepted" => CoordinationStatus::Accepted,
        "decline" | "declined" => CoordinationStatus::Declined,
        "moved" => CoordinationStatus::Moved,
        other => {
            return Err(McpError::invalid_params(
                format!("invalid response '{other}': expected accept | decline | moved"),
                None,
            ));
        }
    };

    let ok = crate::deps::coord_store::respond(
        pool,
        params.request_id,
        status,
        params.editor_session.as_deref(),
        params.worktree_branch.as_deref(),
    )
    .await
    .map_err(|e| McpError::internal_error(format!("respond failed: {e}"), None))?;
    if !ok {
        return Err(McpError::invalid_params(
            format!(
                "coordination #{} not found or already resolved/cancelled",
                params.request_id
            ),
            None,
        ));
    }

    // Notify the requester of the response — as a TYPED mailbox kind
    // (accept|decline|moved), threaded under the original `request_worktree`
    // message, so the coordination's message thread is a faithful
    // WorktreeNegotiation transcript (lift-checkable via `csm_validate_run`).
    let req: Option<(Option<i32>, Option<String>, Option<i64>)> = sqlx::query_as(
        "SELECT dependent_project_id, requester_session, message_id
           FROM coordination_requests WHERE id = $1",
    )
    .bind(params.request_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    if let Some((dep_pid, req_session, request_msg_id)) = req {
        let wt = params
            .worktree_branch
            .as_deref()
            .map(|b| format!(" Worktree branch: {b}."))
            .unwrap_or_default();
        let body = format!(
            "Editor responded '{}' to your worktree request (coordination #{}).{}",
            status.as_str(),
            params.request_id,
            wt
        );
        let kind = match status {
            CoordinationStatus::Accepted => crate::a2a::mailbox::MessageKind::Accept,
            CoordinationStatus::Declined => crate::a2a::mailbox::MessageKind::Decline,
            CoordinationStatus::Moved => crate::a2a::mailbox::MessageKind::Moved,
            _ => crate::a2a::mailbox::MessageKind::Fyi,
        };
        let msg = crate::a2a::mailbox_store::NewMessage {
            from_agent: "pgmcp",
            from_session: params.editor_session.as_deref(),
            to_session: req_session.as_deref(),
            to_project_id: dep_pid,
            to_agent: None,
            kind: kind.as_str(),
            subject: Some("coordination response"),
            body: &body,
            reply_to: request_msg_id,
            expires_at: None,
        };
        let _ = crate::a2a::mailbox_store::send(pool, &msg).await;
    }

    debug!(
        tool = "coordination_respond",
        request_id = params.request_id,
        status = status.as_str(),
        "recorded"
    );
    let note = if status == CoordinationStatus::Moved {
        "Recorded as a CANDIDATE — the dependent unblocks only when pgmcp's git scanner observes \
         the dependency back on its stable branch & clean."
    } else {
        ""
    };
    let out = serde_json::to_string_pretty(&json!({
        "request_id": params.request_id,
        "status": status.as_str(),
        "recorded": ok,
        "note": note,
    }))
    .map_err(|e| McpError::internal_error(format!("Serialization failed: {e}"), None))?;
    Ok(CallToolResult::success(vec![Content::text(out)]))
}
