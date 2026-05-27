//! CRUD + hierarchy tool bodies for the work-item tracker: create, get,
//! update, list, tree, reparent. Status transitions live in `lifecycle.rs`.

#![allow(unused_imports)]

use std::sync::atomic::Ordering;

use chrono::{DateTime, Utc};
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::db::queries::{
    self, NewWorkItem, WorkItemFilter, WorkItemOpError, get_work_item, get_work_item_by_public_id,
    get_work_item_subtree, insert_work_item, list_work_items, reparent_work_item,
    resolve_project_id, update_work_item_fields,
};
use crate::mcp::server::{
    WorkItemCreateParams, WorkItemGetParams, WorkItemListParams, WorkItemReparentParams,
    WorkItemTreeParams, WorkItemUpdateParams,
};
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};
use crate::mcp::tools::work_items::gen_public_id;
use crate::tracker::kind::WorkItemKind;

/// Map a fallible-tracker-op error to the right MCP error class: a refused
/// transition or a missing item is a caller mistake (`invalid_params`); a raw
/// DB failure is internal. Shared with the verify/lifecycle tool bodies.
pub(crate) fn map_op_err(e: WorkItemOpError) -> McpError {
    match e {
        WorkItemOpError::Transition(_) | WorkItemOpError::NotFound => {
            McpError::invalid_params(e.to_string(), None)
        }
        WorkItemOpError::Db(_) => McpError::internal_error(e.to_string(), None),
    }
}

/// Map a bare `sqlx::Error` (from the non-`WorkItemOpError` query helpers) to an
/// internal MCP error. Shared with the Phase-2 tag/progress tool bodies.
pub(crate) fn map_db_err(e: sqlx::Error) -> McpError {
    McpError::internal_error(format!("db error: {e}"), None)
}

/// Resolve a `public_id` to its numeric id, erroring with `invalid_params` if
/// no such item exists. Shared with the Phase-2 tag/progress tool bodies.
pub(crate) async fn id_of_public(pool: &sqlx::PgPool, public_id: &str) -> Result<i64, McpError> {
    let row = get_work_item_by_public_id(pool, public_id)
        .await
        .map_err(map_db_err)?;
    row.map(|r| r.id)
        .ok_or_else(|| McpError::invalid_params(format!("no work item '{public_id}'"), None))
}

/// Parse a schedule field (`due_at`/`snooze_until`) param into the query layer's
/// three-way `(set, clear)` form: `None` → leave unchanged; an empty /
/// `none`/`clear`/`null` sentinel → clear (NULL); an RFC3339 timestamp → set.
/// An unparseable timestamp is an `invalid_params` error.
pub(crate) fn parse_schedule_field(
    opt: &Option<String>,
    field: &str,
) -> Result<(Option<DateTime<Utc>>, bool), McpError> {
    match opt.as_deref().map(str::trim) {
        None => Ok((None, false)),
        Some(s)
            if s.is_empty()
                || matches!(s.to_ascii_lowercase().as_str(), "none" | "clear" | "null") =>
        {
            Ok((None, true))
        }
        Some(s) => DateTime::parse_from_rfc3339(s)
            .map(|d| (Some(d.with_timezone(&Utc)), false))
            .map_err(|e| {
                McpError::invalid_params(
                    format!("{field} must be an RFC3339 timestamp (or empty/none to clear): {e}"),
                    None,
                )
            }),
    }
}

// ============================================================================
// work_item_create
// ============================================================================

pub async fn tool_work_item_create(
    ctx: &SystemContext,
    params: WorkItemCreateParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    // Validate the kind against the closed vocabulary. `sql_in_list` is a
    // module-level free function (not an associated item) — see
    // `crate::tracker::kind`.
    let kind = WorkItemKind::parse(&params.kind).ok_or_else(|| {
        McpError::invalid_params(
            format!(
                "unknown kind '{}'; expected one of {}",
                params.kind,
                crate::tracker::kind::sql_in_list()
            ),
            None,
        )
    })?;

    let title = params.title.trim();
    if title.is_empty() {
        return Err(McpError::invalid_params("title must be non-empty", None));
    }

    // Resolve an optional parent public_id to its numeric id.
    let parent_id = match params.parent_public_id.as_deref() {
        None => None,
        Some(p) => Some(id_of_public(pool, p).await?),
    };

    let project_id = resolve_project_id(pool, params.project.as_deref())
        .await
        .map_err(map_db_err)?;

    let public_id = params
        .public_id
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| gen_public_id(title));

    // Embed title+body on write so semantic backlog search works immediately
    // (a transient embed failure is non-fatal — the column stays NULL).
    let embedding = super::embed_title_body(ctx, title, params.body.as_deref()).await;

    let new_item = NewWorkItem {
        public_id: &public_id,
        parent_id,
        project_id,
        kind: kind.as_str(),
        title,
        body: params.body.as_deref(),
        priority: params.priority.unwrap_or(0),
        weight: params.weight.unwrap_or(1.0),
        parametric: params.parametric.unwrap_or(false),
        parametric_corpus: params.parametric_corpus.as_deref(),
        origin: "agent_write",
        embedding,
        ..Default::default()
    };

    let new_id = insert_work_item(pool, new_item).await.map_err(map_db_err)?;
    let row = get_work_item(pool, new_id)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| McpError::internal_error("inserted work item vanished", None))?;

    ctx.stats()
        .work_items_created
        .fetch_add(1, Ordering::Relaxed);
    json_result(&row)
}

// ============================================================================
// work_item_get
// ============================================================================

pub async fn tool_work_item_get(
    ctx: &SystemContext,
    params: WorkItemGetParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let row = get_work_item_by_public_id(pool, &params.public_id)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| {
            McpError::invalid_params(format!("no work item '{}'", params.public_id), None)
        })?;

    if params.include_subtree.unwrap_or(false) {
        let subtree = get_work_item_subtree(pool, row.id, 10_000)
            .await
            .map_err(map_db_err)?;
        json_result(&json!({ "item": row, "subtree": subtree }))
    } else {
        json_result(&json!({ "item": row }))
    }
}

// ============================================================================
// work_item_update
// ============================================================================

pub async fn tool_work_item_update(
    ctx: &SystemContext,
    params: WorkItemUpdateParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let id = id_of_public(pool, &params.public_id).await?;
    let (due_at, clear_due) = parse_schedule_field(&params.due_at, "due_at")?;
    let (snooze_until, clear_snooze) = parse_schedule_field(&params.snooze_until, "snooze_until")?;
    let row = update_work_item_fields(
        pool,
        id,
        params.title.as_deref(),
        params.body.as_deref(),
        params.priority,
        params.weight,
        due_at,
        clear_due,
        snooze_until,
        clear_snooze,
    )
    .await
    .map_err(map_op_err)?;

    json_result(&row)
}

// ============================================================================
// work_item_list
// ============================================================================

pub async fn tool_work_item_list(
    ctx: &SystemContext,
    params: WorkItemListParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let project_id = resolve_project_id(pool, params.project.as_deref())
        .await
        .map_err(map_db_err)?;
    let parent_id = match params.parent_public_id.as_deref() {
        None => None,
        Some(p) => Some(id_of_public(pool, p).await?),
    };

    let filter = WorkItemFilter {
        project_id,
        kind: params.kind.as_deref(),
        status: params.status.as_deref(),
        parent_id,
        overdue: params.overdue.unwrap_or(false),
        include_snoozed: params.include_snoozed.unwrap_or(false),
        limit: params.limit.unwrap_or(50),
    };
    let rows = list_work_items(pool, &filter).await.map_err(map_db_err)?;

    ctx.stats()
        .work_item_queries
        .fetch_add(1, Ordering::Relaxed);
    json_result(&rows)
}

// ============================================================================
// work_item_tree
// ============================================================================

pub async fn tool_work_item_tree(
    ctx: &SystemContext,
    params: WorkItemTreeParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let id = id_of_public(pool, &params.public_id).await?;
    let rows = get_work_item_subtree(pool, id, params.max_rows.unwrap_or(10_000))
        .await
        .map_err(map_db_err)?;

    json_result(&rows)
}

// ============================================================================
// work_item_reparent
// ============================================================================

pub async fn tool_work_item_reparent(
    ctx: &SystemContext,
    params: WorkItemReparentParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let id = id_of_public(pool, &params.public_id).await?;
    let new_parent_id = match params.new_parent_public_id.as_deref() {
        None => None,
        Some(p) => Some(id_of_public(pool, p).await?),
    };

    // Cycle guard: the new parent may be neither the item itself nor any of its
    // descendants (that would orphan the moved subtree into a loop). Fetch the
    // item's subtree and reject if the proposed parent is inside it.
    if let Some(np) = new_parent_id {
        if np == id {
            return Err(McpError::invalid_params(
                "cannot reparent an item under itself",
                None,
            ));
        }
        let subtree = get_work_item_subtree(pool, id, 100_000)
            .await
            .map_err(map_db_err)?;
        if subtree.iter().any(|r| r.id == np) {
            return Err(McpError::invalid_params(
                "cannot reparent an item under one of its own descendants (would create a cycle)",
                None,
            ));
        }
    }

    reparent_work_item(pool, id, new_parent_id)
        .await
        .map_err(map_op_err)?;

    // Re-fetch to return the updated row (root_id/parent_id now reflect the move).
    let row = get_work_item(pool, id)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| McpError::internal_error("reparented work item vanished", None))?;
    json_result(&row)
}
