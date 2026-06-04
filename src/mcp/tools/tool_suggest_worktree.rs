//! `tool_suggest_worktree` — suggest the exact git commands for an editor to
//! move its in-flight edits to a worktree on a feature branch and restore the
//! project's stable branch (so dependents are unblocked). pgmcp SUGGESTS; it
//! never runs git (the trust boundary — read-only git).

#![allow(unused_imports)]

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::json;
use tracing::debug;

use crate::context::SystemContext;
use crate::mcp::server::*;

pub async fn tool_suggest_worktree(
    ctx: &SystemContext,
    params: SuggestWorktreeParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let Some(pool) = ctx.db().pool() else {
        return Err(McpError::internal_error(
            "database pool unavailable".to_string(),
            None,
        ));
    };

    let row: Option<(i32, String, Option<String>)> =
        sqlx::query_as("SELECT id, path, stable_branch FROM projects WHERE name = $1")
            .bind(&params.project)
            .fetch_optional(pool)
            .await
            .map_err(|e| McpError::internal_error(format!("project lookup: {e}"), None))?;
    let Some((project_id, path, stable_branch)) = row else {
        return Err(McpError::invalid_params(
            format!("unknown project '{}'", params.project),
            None,
        ));
    };

    // Pending coordination requests against this project — so the editor sees who
    // is blocked on it and the request ids to answer via `coordination_respond`.
    let pending = crate::deps::coord_store::open_for_dependency(pool, project_id)
        .await
        .unwrap_or_default();
    let pending_coordinations: Vec<serde_json::Value> = pending
        .iter()
        .map(|c| {
            json!({
                "request_id": c.id,
                "status": c.status,
                "dependent_project_id": c.dependent_project_id,
                "requester_agent": c.requester_agent,
                "reason": c.reason,
                "error_excerpt": c.error_excerpt,
                "opened_at": c.created_at.to_rfc3339(),
            })
        })
        .collect();
    let path = path.trim_end_matches('/').to_string();
    let stable = stable_branch.as_deref().unwrap_or("main");
    let feat = params.feature_branch.as_deref().unwrap_or("wip");

    // Move the in-flight work onto a feature branch, restore the stable branch in
    // the main tree, and continue the work in a separate worktree. Preserves the
    // uncommitted edits (they ride the new branch).
    let commands = format!(
        "git -C {path} switch -c {feat}\n\
         git -C {path} add -A && git -C {path} commit -m \"wip: move off {stable} for coordination\"\n\
         git -C {path} switch {stable}\n\
         git -C {path} worktree add ../{worktree_dir} {feat}\n\
         # continue your work in:  {path}/../{worktree_dir}",
        path = path,
        feat = feat,
        stable = stable,
        worktree_dir = format!(
            "{}-{}",
            std::path::Path::new(&path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("project"),
            feat
        ),
    );

    debug!(tool = "suggest_worktree", project = %params.project, "suggested worktree commands");
    let out = serde_json::to_string_pretty(&json!({
        "project": params.project,
        "path": path,
        "stable_branch": stable,
        "feature_branch": feat,
        "commands": commands,
        "pending_coordination_count": pending_coordinations.len(),
        "pending_coordinations": pending_coordinations,
        "note": "pgmcp SUGGESTS these commands; it never runs git. Running them moves your \
                 in-flight edits onto a feature branch (in a worktree) and restores the stable \
                 branch in the main tree — pgmcp's git scanner will then detect the restore and \
                 auto-resolve the coordination, unblocking the dependent.",
    }))
    .map_err(|e| McpError::internal_error(format!("Serialization failed: {e}"), None))?;
    Ok(CallToolResult::success(vec![Content::text(out)]))
}
