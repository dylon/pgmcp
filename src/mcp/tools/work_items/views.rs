//! Phase-2 smart-view + next-action tool bodies (`~/.claude/plans/
//! how-extensive-is-the-zazzy-galaxy.md`, "Tracker ergonomics & next-action"):
//!
//! - `work_item_view` — one of the five fixed [`SmartView`] queues over the
//!   existing `list_work_items` path (no `saved_views` table; the set is closed).
//! - `work_item_next_actionable` — the read-only "what can I do now" frontier
//!   (the un-claiming sibling of `claim_next`'s `NOT_BLOCKED` SELECT).
//!
//! Both are READ-ONLY. The `view → WorkItemFilter` resolution lives in
//! [`view_filter`] here and is reused by `bulk::tool_work_item_bulk` so a
//! `work_item_bulk { view, … }` selects exactly the same targets a
//! `work_item_view` would list.

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::db::queries::{WorkItemFilter, list_work_items, next_actionable_work_items};
use crate::mcp::server::{WorkItemNextActionableParams, WorkItemViewParams};
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};
use crate::mcp::tools::work_items::crud::{id_of_public, map_db_err};
use crate::tracker::views::SmartView;

/// Build the [`WorkItemFilter`] that realizes a [`SmartView`]. The `assignee`
/// is borrowed (the caller owns the resolved string) and is only consulted for
/// [`SmartView::MyWork`]; every other view ignores it. `limit` is applied
/// verbatim (the query layer clamps it).
///
/// Shared by `work_item_view` and `work_item_bulk` so a view selects the same
/// targets in both — the single source of truth for the closed view semantics.
pub(crate) fn view_filter<'a>(
    view: SmartView,
    assignee: Option<&'a str>,
    limit: i64,
) -> WorkItemFilter<'a> {
    // Each arm sets exactly the one facet that defines the view; the rest fall
    // back to `WorkItemFilter::default()` (all unconstrained, snoozed hidden).
    match view {
        SmartView::MyWork => WorkItemFilter {
            assignee,
            limit,
            ..Default::default()
        },
        SmartView::NeedsTriage => WorkItemFilter {
            needs_triage: true,
            limit,
            ..Default::default()
        },
        SmartView::Overdue => WorkItemFilter {
            overdue: true,
            limit,
            ..Default::default()
        },
        SmartView::Blocked => WorkItemFilter {
            status: Some("blocked"),
            limit,
            ..Default::default()
        },
        SmartView::NextActionable => WorkItemFilter {
            next_actionable: true,
            limit,
            ..Default::default()
        },
    }
}

/// Parse the `view` param against the closed [`SmartView`] set, returning an
/// `invalid_params` listing the whole vocabulary on a miss. Shared with
/// `work_item_bulk`.
pub(crate) fn parse_view(raw: &str) -> Result<SmartView, McpError> {
    SmartView::parse(raw.trim()).ok_or_else(|| {
        let allowed = SmartView::ALL
            .iter()
            .map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        McpError::invalid_params(
            format!("unknown view '{raw}'; expected one of {allowed}"),
            None,
        )
    })
}

/// `work_item_view` — list one of the five built-in smart-view queues.
pub async fn tool_work_item_view(
    ctx: &SystemContext,
    params: WorkItemViewParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .work_item_queries
        .fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let view = parse_view(&params.view)?;
    let limit = params.limit.unwrap_or(50);

    // my-work scopes to the caller's durable assignee. The MCP `#[tool]` method
    // fills `assignee` from the client name when omitted; the CLI path has no
    // RequestContext, so fall back to the "cli" sentinel so the view is still
    // well-defined (and empty rather than erroring) for direct dispatch.
    let owned_assignee: Option<String> = match view {
        SmartView::MyWork => Some(
            params
                .assignee
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("cli")
                .to_string(),
        ),
        _ => None,
    };

    let filter = view_filter(view, owned_assignee.as_deref(), limit);
    let items = list_work_items(pool, &filter).await.map_err(map_db_err)?;

    json_result(&json!({
        "view": view.as_str(),
        "count": items.len(),
        "items": items,
    }))
}

/// `work_item_next_actionable` — the read-only "what can I do now" frontier:
/// actionable-status items with every blocker cleared, optionally scoped to a
/// plan subtree and/or a durable assignee.
pub async fn tool_work_item_next_actionable(
    ctx: &SystemContext,
    params: WorkItemNextActionableParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .work_item_queries
        .fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let plan_root_id = match params.plan_public_id.as_deref() {
        None => None,
        Some(p) => Some(id_of_public(pool, p).await?),
    };
    let assignee = params
        .assignee
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let limit = params.limit.unwrap_or(50);

    let actionable = next_actionable_work_items(pool, plan_root_id, assignee, limit)
        .await
        .map_err(map_db_err)?;

    json_result(&json!({
        "count": actionable.len(),
        "actionable": actionable,
    }))
}
