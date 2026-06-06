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
    BugDetailFields, NewWorkItem, WorkItemFilter, WorkItemOpError, fetch_bug_details,
    get_work_item, get_work_item_by_public_id, get_work_item_subtree, insert_work_item_in_tx,
    list_work_items, reparent_work_item, update_work_item_fields_in_tx, upsert_bug_details_in_tx,
};
use crate::mcp::server::{
    WorkItemCreateParams, WorkItemGetParams, WorkItemListParams, WorkItemReparentParams,
    WorkItemTreeParams, WorkItemUpdateParams,
};
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};
use crate::mcp::tools::work_items::{gen_public_id, nonblank};
use crate::tracker::kind::WorkItemKind;
use crate::tracker::severity::Severity;
use crate::tracker::status::WorkItemStatus;

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

fn map_create_db_err(e: sqlx::Error) -> McpError {
    if matches!(&e, sqlx::Error::RowNotFound) {
        return McpError::invalid_params("referenced parent no longer exists", None);
    }
    if let Some(db) = e.as_database_error() {
        let code = db.code().map(|code| code.into_owned());
        return match code.as_deref() {
            Some("23505") => McpError::invalid_params("public_id already exists", None),
            Some("23503") => {
                McpError::invalid_params("referenced parent or project no longer exists", None)
            }
            Some("23514") => McpError::invalid_params("work item rejected by DB constraint", None),
            _ => McpError::internal_error(format!("db error: {e}"), None),
        };
    }
    map_db_err(e)
}

pub(crate) async fn resolve_existing_project_id_param(
    pool: &sqlx::PgPool,
    project: Option<&str>,
) -> Result<Option<i32>, McpError> {
    let Some(name) = project.map(str::trim).filter(|name| !name.is_empty()) else {
        return Ok(None);
    };
    let rows =
        sqlx::query_scalar::<_, i32>("SELECT id FROM projects WHERE name = $1 ORDER BY id LIMIT 2")
            .bind(name)
            .fetch_all(pool)
            .await
            .map_err(map_db_err)?;
    match rows.as_slice() {
        [] => Err(McpError::invalid_params(
            format!("unknown project '{name}'"),
            None,
        )),
        [id] => Ok(Some(*id)),
        _ => Err(McpError::invalid_params(
            format!("project name '{name}' is ambiguous"),
            None,
        )),
    }
}

fn required_nonblank<'a>(value: &'a str, field: &str) -> Result<&'a str, McpError> {
    let value = value.trim();
    if value.is_empty() {
        Err(McpError::invalid_params(
            format!("{field} must be non-empty"),
            None,
        ))
    } else {
        Ok(value)
    }
}

/// Resolve a `public_id` to its numeric id, erroring with `invalid_params` if
/// no such item exists. Shared with the Phase-2 tag/progress tool bodies.
pub(crate) async fn id_of_public(pool: &sqlx::PgPool, public_id: &str) -> Result<i64, McpError> {
    let public_id = required_nonblank(public_id, "public_id")?;
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
    let kind_raw = params.kind.trim();
    let kind = WorkItemKind::parse(kind_raw).ok_or_else(|| {
        McpError::invalid_params(
            format!(
                "unknown kind '{}'; expected one of {}",
                kind_raw,
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
    let parent_id = match nonblank(&params.parent_public_id) {
        None => None,
        Some(p) => Some(id_of_public(pool, p).await?),
    };

    let project_id = resolve_existing_project_id_param(pool, params.project.as_deref()).await?;

    let public_id = nonblank(&params.public_id)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| gen_public_id(title));
    let body = params
        .body
        .as_deref()
        .filter(|body| !body.trim().is_empty());

    // Bug fields. Severity is validated against the closed vocabulary; a bug is
    // born in `triage` (awaiting a user-token confirmation); and a severity with
    // no explicit priority seeds a default urgency (never clobbering an explicit
    // priority).
    let is_bug = kind == WorkItemKind::Bug;
    let bug_field_supplied = nonblank(&params.severity).is_some()
        || nonblank(&params.reproduction_steps).is_some()
        || nonblank(&params.expected_behavior).is_some()
        || nonblank(&params.actual_behavior).is_some()
        || nonblank(&params.environment).is_some()
        || nonblank(&params.affected_version).is_some()
        || params.is_regression.is_some()
        || nonblank(&params.reported_by).is_some();
    if !is_bug && bug_field_supplied {
        return Err(McpError::invalid_params(
            "severity and structured bug fields require kind='bug'",
            None,
        ));
    }
    let severity = match nonblank(&params.severity) {
        Some(s) => Some(Severity::parse(s).ok_or_else(|| {
            McpError::invalid_params(
                format!(
                    "unknown severity '{s}'; expected one of {}",
                    crate::tracker::severity::sql_in_list()
                ),
                None,
            )
        })?),
        None => None,
    };
    let status = if is_bug { "triage" } else { "pending" };
    let priority = params
        .priority
        .or_else(|| severity.map(Severity::default_priority))
        .unwrap_or(0);
    if !(0..=100).contains(&priority) {
        return Err(McpError::invalid_params(
            "priority must be between 0 and 100",
            None,
        ));
    }
    let weight = params.weight.unwrap_or(1.0);
    if !weight.is_finite() || weight <= 0.0 {
        return Err(McpError::invalid_params(
            "weight must be a positive finite number",
            None,
        ));
    }

    // Fold the descriptive bug text into the embedding input so "find similar
    // bugs" semantic search sees reproduction / expected-vs-actual. (root_cause
    // is set later, during triage; the cron's work_items backfill composes it
    // from the sidecar.)
    let bug_embed_extra: Option<String> = {
        let parts: Vec<&str> = [
            nonblank(&params.reproduction_steps),
            nonblank(&params.expected_behavior),
            nonblank(&params.actual_behavior),
        ]
        .into_iter()
        .flatten()
        .collect();
        (!parts.is_empty()).then(|| parts.join("\n"))
    };
    let embedding = super::embed_title_body(ctx, title, body, bug_embed_extra.as_deref()).await;

    let new_item = NewWorkItem {
        public_id: &public_id,
        parent_id,
        project_id,
        kind: kind.as_str(),
        status,
        title,
        body,
        priority,
        weight,
        parametric: params.parametric.unwrap_or(false),
        parametric_corpus: nonblank(&params.parametric_corpus),
        origin: "agent_write",
        severity: severity.map(Severity::as_str),
        embedding,
        ..Default::default()
    };

    // Persist the structured bug-detail sidecar only for first-class bugs.
    let bug_fields = BugDetailFields {
        reproduction_steps: nonblank(&params.reproduction_steps),
        expected_behavior: nonblank(&params.expected_behavior),
        actual_behavior: nonblank(&params.actual_behavior),
        environment: nonblank(&params.environment),
        affected_version: nonblank(&params.affected_version),
        is_regression: params.is_regression,
        reported_by: nonblank(&params.reported_by),
        ..Default::default()
    };

    let mut tx = pool.begin().await.map_err(map_db_err)?;
    let new_id = insert_work_item_in_tx(&mut tx, &new_item)
        .await
        .map_err(map_create_db_err)?;
    if is_bug {
        upsert_bug_details_in_tx(&mut tx, new_id, &bug_fields)
            .await
            .map_err(map_create_db_err)?;
    }
    tx.commit().await.map_err(map_db_err)?;

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

    let public_id = required_nonblank(&params.public_id, "public_id")?;
    let row = get_work_item_by_public_id(pool, public_id)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| McpError::invalid_params(format!("no work item '{public_id}'"), None))?;

    // Include the bug-detail sidecar (NULL for non-bug items).
    let bug_details = fetch_bug_details(pool, row.id).await.map_err(map_db_err)?;
    if params.include_subtree.unwrap_or(false) {
        let subtree = get_work_item_subtree(pool, row.id, 10_000)
            .await
            .map_err(map_db_err)?;
        json_result(&json!({ "item": row, "bug_details": bug_details, "subtree": subtree }))
    } else {
        json_result(&json!({ "item": row, "bug_details": bug_details }))
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
    let current = get_work_item(pool, id)
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| McpError::invalid_params("work item not found", None))?;
    let title = match params.title.as_deref() {
        None => None,
        Some(title) => Some(required_nonblank(title, "title")?),
    };
    let body = params.body.as_deref().map(str::trim);
    if let Some(priority) = params.priority
        && !(0..=100).contains(&priority)
    {
        return Err(McpError::invalid_params(
            "priority must be between 0 and 100",
            None,
        ));
    }
    if let Some(weight) = params.weight
        && (!weight.is_finite() || weight <= 0.0)
    {
        return Err(McpError::invalid_params(
            "weight must be a positive finite number",
            None,
        ));
    }
    let (due_at, clear_due) = parse_schedule_field(&params.due_at, "due_at")?;
    let (snooze_until, clear_snooze) = parse_schedule_field(&params.snooze_until, "snooze_until")?;
    let bug_field_supplied = nonblank(&params.severity).is_some()
        || nonblank(&params.reproduction_steps).is_some()
        || nonblank(&params.expected_behavior).is_some()
        || nonblank(&params.actual_behavior).is_some()
        || nonblank(&params.environment).is_some()
        || nonblank(&params.affected_version).is_some()
        || nonblank(&params.fixed_in_version).is_some()
        || nonblank(&params.root_cause).is_some()
        || params.is_regression.is_some();
    if current.kind != WorkItemKind::Bug.as_str() && bug_field_supplied {
        return Err(McpError::invalid_params(
            "severity and structured bug fields require kind='bug'",
            None,
        ));
    }
    let severity = match nonblank(&params.severity) {
        Some(s) => Some(Severity::parse(s).ok_or_else(|| {
            McpError::invalid_params(
                format!(
                    "unknown severity '{s}'; expected one of {}",
                    crate::tracker::severity::sql_in_list()
                ),
                None,
            )
        })?),
        None => None,
    };

    // Fill in any structured bug fields supplied alongside the update.
    let bug_fields = BugDetailFields {
        reproduction_steps: nonblank(&params.reproduction_steps),
        expected_behavior: nonblank(&params.expected_behavior),
        actual_behavior: nonblank(&params.actual_behavior),
        environment: nonblank(&params.environment),
        affected_version: nonblank(&params.affected_version),
        fixed_in_version: nonblank(&params.fixed_in_version),
        root_cause: nonblank(&params.root_cause),
        is_regression: params.is_regression,
        ..Default::default()
    };
    let mut tx = pool.begin().await.map_err(map_db_err)?;
    let row = update_work_item_fields_in_tx(
        &mut tx,
        id,
        title,
        body,
        params.priority,
        params.weight,
        due_at,
        clear_due,
        snooze_until,
        clear_snooze,
        severity.map(Severity::as_str),
    )
    .await
    .map_err(map_op_err)?;
    if !bug_fields.is_empty() {
        upsert_bug_details_in_tx(&mut tx, id, &bug_fields)
            .await
            .map_err(map_db_err)?;
    }
    tx.commit().await.map_err(map_db_err)?;

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

    let project_id = resolve_existing_project_id_param(pool, params.project.as_deref()).await?;
    let parent_id = match nonblank(&params.parent_public_id) {
        None => None,
        Some(p) => Some(id_of_public(pool, p).await?),
    };
    let kind = match nonblank(&params.kind) {
        None => None,
        Some(kind) => Some(WorkItemKind::parse(kind).ok_or_else(|| {
            McpError::invalid_params(
                format!(
                    "unknown kind '{kind}'; expected one of {}",
                    crate::tracker::kind::sql_in_list()
                ),
                None,
            )
        })?),
    };
    let status = match nonblank(&params.status) {
        None => None,
        Some(status) => Some(WorkItemStatus::parse(status).ok_or_else(|| {
            McpError::invalid_params(
                format!(
                    "unknown status '{status}'; expected one of {}",
                    crate::tracker::status::sql_in_list()
                ),
                None,
            )
        })?),
    };

    let filter = WorkItemFilter {
        project_id,
        kind: kind.map(WorkItemKind::as_str),
        status: status.map(WorkItemStatus::as_str),
        parent_id,
        overdue: params.overdue.unwrap_or(false),
        include_snoozed: params.include_snoozed.unwrap_or(false),
        limit: params.limit.unwrap_or(50),
        ..Default::default()
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

    ctx.stats()
        .work_item_queries
        .fetch_add(1, Ordering::Relaxed);
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
