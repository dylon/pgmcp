//! `work_item_bulk` — apply one [`BulkOp`] to many items at once
//! (`~/.claude/plans/how-extensive-is-the-zazzy-galaxy.md`, "Tracker ergonomics
//! & next-action").
//!
//! Targets are selected EITHER by explicit `public_ids` OR by a [`SmartView`]
//! (reusing [`super::views::view_filter`], so a bulk-by-view operates on exactly
//! the set `work_item_view` would list). Targets are capped at 500.
//!
//! TRUST BOUNDARY: `op=set_status` loops through the per-item
//! [`queries::set_work_item_status`] chokepoint as [`Actor::Agent`] — NEVER an
//! actor read from params, never `System`/`Gatekeeper`. So bulk inherits, per
//! item, the full transition-legality gate (an illegal transition lands in
//! `failed`, not applied) AND the auto-unblock cascade (verifying a blocker via
//! bulk still auto-readies its dependents). `assign` is NOT a transition. The
//! envelope is partial-success: `{op, attempted, succeeded, failed:[…]}`.

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::db::queries::{
    self, assign_work_item, get_tag_by_slug, tag_work_item, untag_work_item,
    update_work_item_fields, upsert_tag,
};
use crate::mcp::server::WorkItemBulkParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};
use crate::mcp::tools::work_items::crud::{id_of_public, map_db_err, map_op_err};
use crate::mcp::tools::work_items::slugify;
use crate::mcp::tools::work_items::views::{parse_view, view_filter};
use crate::tracker::status::WorkItemStatus;
use crate::tracker::transition::Actor;
use crate::tracker::views::BulkOp;

/// Upper bound on the number of items one bulk call may touch (protects the
/// per-item transaction loop from a runaway view/`public_ids` set).
const BULK_CAP: usize = 500;

/// `work_item_bulk` — apply `op` to every resolved target, collecting per-item
/// failures rather than aborting on the first.
pub async fn tool_work_item_bulk(
    ctx: &SystemContext,
    params: WorkItemBulkParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let op = BulkOp::parse(params.op.trim()).ok_or_else(|| {
        let allowed = BulkOp::ALL
            .iter()
            .map(|o| o.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        McpError::invalid_params(
            format!("unknown bulk op '{}'; expected one of {allowed}", params.op),
            None,
        )
    })?;

    // ── Resolve targets to numeric ids. Explicit public_ids win; otherwise a
    //    SmartView selects them via the shared view→filter resolution. A
    //    completely empty selector is a caller mistake. ──
    let target_ids: Vec<(String, i64)> = match params.public_ids.as_ref() {
        Some(ids) if !ids.is_empty() => {
            if ids.len() > BULK_CAP {
                return Err(McpError::invalid_params(
                    format!("too many targets ({}); cap is {BULK_CAP}", ids.len()),
                    None,
                ));
            }
            let mut out = Vec::with_capacity(ids.len());
            for pid in ids {
                let id = id_of_public(pool, pid).await?;
                out.push((pid.clone(), id));
            }
            out
        }
        _ => {
            let raw_view = params.view.as_deref().ok_or_else(|| {
                McpError::invalid_params("select targets with either public_ids or view", None)
            })?;
            let view = parse_view(raw_view)?;
            // my-work-by-bulk needs an explicit assignee (no caller identity on a
            // bulk op); reuse the assignee param as the scope when present.
            let assignee = params
                .assignee
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let filter = view_filter(view, assignee, BULK_CAP as i64);
            let rows = queries::list_work_items(pool, &filter)
                .await
                .map_err(map_db_err)?;
            rows.into_iter().map(|r| (r.public_id, r.id)).collect()
        }
    };

    // ── Pre-resolve op-specific inputs once (so a bad status/tag/missing field
    //    fails fast, before any mutation). ──
    let reason = params
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let parsed_status = match op {
        BulkOp::SetStatus => {
            let raw = params.status.as_deref().map(str::trim).unwrap_or("");
            Some(WorkItemStatus::parse(raw).ok_or_else(|| {
                McpError::invalid_params(
                    format!(
                        "op=set_status requires a valid status; got '{raw}'. Expected one of {}",
                        crate::tracker::status::sql_in_list()
                    ),
                    None,
                )
            })?)
        }
        _ => None,
    };

    // For tag/untag, resolve (and for tag, auto-create) the tag once: every
    // target shares the same tag id.
    let tag_id = match op {
        BulkOp::Tag | BulkOp::Untag => {
            let label = params
                .tag
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    McpError::invalid_params(
                        format!("op={} requires a non-empty tag", op.as_str()),
                        None,
                    )
                })?;
            let slug = slugify(label);
            match op {
                BulkOp::Tag => {
                    // Auto-create the tag so a bulk-tag never silently skips.
                    upsert_tag(pool, label, &slug, None, None)
                        .await
                        .map_err(map_db_err)?;
                    let t = get_tag_by_slug(pool, &slug)
                        .await
                        .map_err(map_db_err)?
                        .ok_or_else(|| McpError::internal_error("upserted tag vanished", None))?;
                    Some(t.id)
                }
                // Untag: an unknown tag is a no-op set (nothing to remove).
                _ => get_tag_by_slug(pool, &slug)
                    .await
                    .map_err(map_db_err)?
                    .map(|t| t.id),
            }
        }
        _ => None,
    };

    let priority = match op {
        BulkOp::Reprioritize => Some(params.priority.ok_or_else(|| {
            McpError::invalid_params("op=reprioritize requires a priority", None)
        })?),
        _ => None,
    };

    let assignee_for_assign = params
        .assignee
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    // ── Apply per item, collecting failures. ──
    let attempted = target_ids.len();
    let mut succeeded = 0usize;
    let mut failed: Vec<serde_json::Value> = Vec::new();

    for (public_id, id) in target_ids {
        let result: Result<(), McpError> = match op {
            // TRUST: per-item status change ALWAYS as Actor::Agent through the
            // chokepoint (legality + auto-unblock fire per item).
            BulkOp::SetStatus => {
                let status = parsed_status.expect("set_status validated above");
                queries::set_work_item_status(
                    pool,
                    id,
                    status,
                    Actor::Agent,
                    None,
                    reason,
                    None,
                    None,
                )
                .await
                .map(|_| ())
                .map_err(map_op_err)
            }
            BulkOp::Tag => {
                let tid = tag_id.expect("tag id resolved above");
                tag_work_item(pool, id, tid, None)
                    .await
                    .map(|_| ())
                    .map_err(map_db_err)
            }
            BulkOp::Untag => match tag_id {
                Some(tid) => untag_work_item(pool, id, tid)
                    .await
                    .map(|_| ())
                    .map_err(map_db_err),
                // No such tag exists ⇒ nothing to remove; not a failure.
                None => Ok(()),
            },
            BulkOp::Reprioritize => {
                let p = priority.expect("priority validated above");
                update_work_item_fields(
                    pool,
                    id,
                    None,
                    None,
                    Some(p),
                    None,
                    None,
                    false,
                    None,
                    false,
                    None,
                )
                .await
                .map(|_| ())
                .map_err(map_op_err)
            }
            BulkOp::Assign => assign_work_item(pool, id, assignee_for_assign, None)
                .await
                .map(|_| ())
                .map_err(map_op_err),
        };

        match result {
            Ok(()) => succeeded += 1,
            Err(e) => failed.push(json!({ "public_id": public_id, "error": e.to_string() })),
        }
    }

    json_result(&json!({
        "op": op.as_str(),
        "attempted": attempted,
        "succeeded": succeeded,
        "failed": failed,
    }))
}
