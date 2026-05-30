//! `work_item_link_commit` — manually link a work item to a commit / PR /
//! branch.
//!
//! This is the agent/user-facing counterpart to the indexer's auto-linkage
//! (`crate::indexer::git_indexer`). It is a pure *link* operation: it records a
//! `work_item_git_links` row (`detected_by='manual'`) and, for a commit link,
//! resolves the SHA to a `git_commits.id` when the commit has been indexed. It
//! does NOT transition the item — advancing status on repo activity is the
//! indexer's `Actor::Agent` path (which can at most reach a verify *candidate*),
//! never a manual tool that an agent could lean on to fabricate progress.
//!
//! Re-linking the same `(item, link_type, ref_value)` is idempotent (the
//! `UNIQUE` constraint + the `ON CONFLICT … RETURNING (xmax=0)` idiom): it
//! returns the existing row with `created=false`.

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::db::queries::{self, get_work_item_by_public_id, resolve_project_id};
use crate::mcp::server::WorkItemLinkCommitParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};
use crate::mcp::tools::work_items::crud::map_db_err;
use crate::tracker::git_link::GitLinkType;

pub async fn tool_work_item_link_commit(
    ctx: &SystemContext,
    params: WorkItemLinkCommitParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let ref_value = params.ref_value.trim();
    if ref_value.is_empty() {
        return Err(McpError::invalid_params(
            "ref_value must be non-empty",
            None,
        ));
    }

    // Resolve / infer the link type. An explicit link_type is validated against
    // the closed vocabulary; omitting it infers from the ref_value's shape.
    let link_type = match params.link_type.as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => GitLinkType::parse(s).ok_or_else(|| {
            McpError::invalid_params(
                format!(
                    "unknown link_type '{s}'; expected one of {}",
                    crate::tracker::git_link::sql_in_list()
                ),
                None,
            )
        })?,
        _ => GitLinkType::infer_from_ref(ref_value),
    };

    let item = get_work_item_by_public_id(pool, &params.public_id)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| {
            McpError::invalid_params(format!("no work item '{}'", params.public_id), None)
        })?;

    // For a commit link, resolve the SHA to a git_commits.id (project-scoped).
    // The project defaults to the item's own project; an explicit `project`
    // param overrides (e.g. linking a monorepo item to a sibling project's
    // commit). A non-indexed / ambiguous SHA leaves commit_id NULL — the link
    // is still recorded by ref_value, and a later indexer pass can resolve it.
    let scope_project_id = match params.project.as_deref() {
        Some(name) => resolve_project_id(pool, Some(name))
            .await
            .map_err(map_db_err)?,
        None => item.project_id,
    };
    let commit_id = if link_type == GitLinkType::Commit {
        match scope_project_id {
            Some(pid) => queries::resolve_commit_id(pool, pid, ref_value)
                .await
                .map_err(map_db_err)?,
            None => None,
        }
    } else {
        None
    };

    let (link_id, created) = queries::insert_git_link(
        pool,
        item.id,
        scope_project_id,
        link_type.as_str(),
        ref_value,
        commit_id,
        "manual",
        Some("agent"),
    )
    .await
    .map_err(map_db_err)?;

    json_result(&json!({
        "link_id": link_id,
        "created": created,
        "item_public_id": item.public_id,
        "link_type": link_type.as_str(),
        "ref_value": ref_value,
        "commit_id": commit_id,
        "commit_resolved": commit_id.is_some(),
        "note": "Manual link only — status is NOT changed. Repo activity advances an item \
                 via the indexer's agent-grade auto-transition (at most to a verify candidate); \
                 →verified still requires CI evidence.",
    }))
}
