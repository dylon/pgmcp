//! `tool_coordinate_dependency_block` — the compile-failure-driven entry to the
//! worktree-coordination protocol. The MCP client knows its own deps, so when
//! its build breaks naming a dependency, it calls this: pgmcp finds the agents
//! live on that dependency, opens a coordination request, and sends them a
//! typed `request_worktree` message. Works even where the derived dependency
//! graph is incomplete (the compiler is ground truth).

#![allow(unused_imports)]

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::json;
use tracing::debug;

use crate::context::SystemContext;
use crate::mcp::server::*;
use crate::mcp::tools::sota_helpers::{pool_or_err, project_id_or_err};

pub async fn tool_coordinate_dependency_block(
    ctx: &SystemContext,
    params: CoordinateDependencyBlockParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    // Resolve the dependency (U) by name — fail closed on blank/unknown/duplicate.
    let u_id = project_id_or_err(ctx, &params.dependency).await?;

    // Resolve the dependent (D) by name, if given. When present and non-blank
    // after trimming, resolve it FAIL-CLOSED (propagate not-found/duplicate)
    // rather than silently dropping an unresolvable name.
    let d_id: Option<i32> = match params.dependent_project.as_deref().map(str::trim) {
        Some(dp) if !dp.is_empty() => Some(project_id_or_err(ctx, dp).await?),
        _ => None,
    };

    // Record the asserted edge (compiler ground truth) so the dependency graph
    // learns it even if no manifest declared it.
    if let Some(dep) = d_id {
        let _ = crate::deps::store::upsert_dependency(
            pool,
            dep,
            u_id,
            Some(&params.dependency),
            None,
            crate::deps::DepSource::Asserted,
            0.9,
        )
        .await;
    }

    // Who is live on the dependency?
    let editors = crate::db::queries::active_agents_by_project(pool, Some(&params.dependency))
        .await
        .unwrap_or_default();

    // Open the coordination request.
    let req_id = crate::deps::coord_store::open_request(
        pool,
        d_id,
        u_id,
        None,
        params.requester_session.as_deref(),
        Some("dependent build broke on this dependency"),
        params.error_excerpt.as_deref(),
        None,
    )
    .await
    .map_err(|e| McpError::internal_error(format!("open coordination: {e}"), None))?;

    // §4.5 work-item gate: if the caller named a blocked work-item, set it
    // `blocked` now (Actor::Agent — the requester owns its own work-item) and link
    // it to this coordination so the git-scanner gatekeeper can later auto-unblock
    // it (`blocked → ready`, Actor::System — the editor can never reach that).
    let mut gated_work_item: Option<String> = None;
    if let Some(pubid) = params.blocked_work_item.as_deref()
        && let Ok(Some(wi)) = crate::db::queries::get_work_item_by_public_id(pool, pubid).await
    {
        let _ = crate::db::queries::set_work_item_status(
            pool,
            wi.id,
            crate::tracker::status::WorkItemStatus::Blocked,
            crate::tracker::transition::Actor::Agent,
            params.requester_session.as_deref(),
            Some("blocked on a dependency being edited (worktree coordination)"),
            None,
            None,
        )
        .await;
        let _ =
            sqlx::query("UPDATE coordination_requests SET blocked_work_item_id = $1 WHERE id = $2")
                .bind(wi.id)
                .bind(req_id)
                .execute(pool)
                .await;
        gated_work_item = Some(wi.public_id);
    }

    // Send the typed `request_worktree` message to anyone editing U.
    let err = params
        .error_excerpt
        .as_deref()
        .map(|e| format!(" Error: {e}"))
        .unwrap_or_default();
    let body = format!(
        "⚠ An agent on {} is blocked: its build broke on dependency '{}'. Please move your \
         in-flight edits on '{}' to a git worktree and restore its stable branch so the \
         dependent is unblocked (coordination #{}). Reply via a2a_reply_message, or use \
         coordination_respond.{}",
        params
            .dependent_project
            .as_deref()
            .unwrap_or("a dependent project"),
        params.dependency,
        params.dependency,
        req_id,
        err,
    );
    let msg = crate::a2a::mailbox_store::NewMessage {
        from_agent: "pgmcp",
        from_session: None,
        to_session: None,
        to_project_id: Some(u_id),
        to_agent: None,
        kind: crate::a2a::mailbox::MessageKind::RequestWorktree.as_str(),
        subject: Some("worktree request — unblock a dependent"),
        body: &body,
        reply_to: None,
        expires_at: None,
    };
    let msg_id = crate::a2a::mailbox_store::send(pool, &msg).await.ok();
    if let Some(mid) = msg_id {
        let _ = sqlx::query("UPDATE coordination_requests SET message_id = $1 WHERE id = $2")
            .bind(mid)
            .bind(req_id)
            .execute(pool)
            .await;
    }

    let active_editors: Vec<serde_json::Value> = editors
        .iter()
        .map(|e| {
            json!({
                "client_name": e.client_name,
                "mcp_session_id": e.mcp_session_id,
                "pid": e.pid,
                "cwd": e.cwd,
            })
        })
        .collect();
    debug!(
        tool = "coordinate_dependency_block",
        request_id = req_id,
        editors = active_editors.len(),
        "opened coordination"
    );
    let out = serde_json::to_string_pretty(&json!({
        "request_id": req_id,
        "dependency": params.dependency,
        "dependency_project_id": u_id,
        "active_editor_count": active_editors.len(),
        "active_editors": active_editors,
        "message_id": msg_id,
        "gated_work_item": gated_work_item,
        "guidance": "The editor(s) on the dependency have been asked to move to a worktree. \
                     When pgmcp's git scanner observes the dependency back on its stable branch \
                     & clean, the request auto-resolves and you'll get an unblocked message. \
                     Only the scanner can resolve it (the editor's 'moved' is a candidate).",
    }))
    .map_err(|e| McpError::internal_error(format!("Serialization failed: {e}"), None))?;
    Ok(CallToolResult::success(vec![Content::text(out)]))
}
