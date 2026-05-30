//! Reporting tools (Phase 9e/9f): a burndown/velocity read over
//! `work_item_status_history`, and a markdown / Org-mode export of a plan
//! subtree. Both are read-only over existing tables.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::Ordering;

use chrono::{Duration, Utc};
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::db::queries::{self, WorkItemRow};
use crate::mcp::server::{WorkItemBurndownParams, WorkItemExportParams, WorkItemHistoryParams};
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};
use crate::mcp::tools::work_items::crud::{id_of_public, map_db_err};

/// `work_item_burndown` — verified-vs-remaining snapshot + realized velocity
/// (items verified/day over the window) + a naive ETA for the plan subtree.
pub async fn tool_work_item_burndown(
    ctx: &SystemContext,
    params: WorkItemBurndownParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .work_item_queries
        .fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let root_id = id_of_public(pool, &params.plan_public_id).await?;
    let window_days = params.window_days.unwrap_or(14).clamp(1, 365);
    let since = Utc::now() - Duration::days(window_days);

    let summary = queries::burndown_summary(pool, root_id)
        .await
        .map_err(map_db_err)?;
    let series = queries::burndown_series(pool, root_id, since)
        .await
        .map_err(map_db_err)?;

    let verified_in_window: i64 = series.iter().map(|d| d.verified).sum();
    let velocity_per_day = verified_in_window as f64 / window_days as f64;
    let remaining = (summary.total - summary.verified).max(0);
    let eta_days = if velocity_per_day > 0.0 {
        Some((remaining as f64 / velocity_per_day).ceil())
    } else {
        None
    };

    // Trajectory (Phase 1): an OLS slope over the *cumulative* verified curve
    // gives a smoothed completion velocity (items/day) that, unlike the flat
    // `velocity_per_day` average, weights the actual day-to-day shape of the
    // window. The series only carries days that had ≥1 verification, so the
    // x-axis is the calendar-day offset from the first such day (gaps honored),
    // and y is the running cumulative count. `regression_eta_days` projects the
    // remaining items onto that fitted slope.
    let mut cumulative = 0i64;
    let mut day0: Option<chrono::NaiveDate> = None;
    let mut fit_points: Vec<(f64, f64)> = Vec::with_capacity(series.len());
    for d in &series {
        cumulative += d.verified;
        if let Ok(date) = chrono::NaiveDate::parse_from_str(&d.day, "%Y-%m-%d") {
            let base = *day0.get_or_insert(date);
            let x = (date - base).num_days() as f64;
            fit_points.push((x, cumulative as f64));
        }
    }
    let slope_per_day = crate::quality::forecast::ols_slope(&fit_points);
    let regression_eta_days = match slope_per_day {
        Some(s) if s > 0.0 => Some((remaining as f64 / s).ceil()),
        _ => None,
    };
    let eta_date = eta_days.map(|d| (Utc::now() + Duration::days(d as i64)).to_rfc3339());
    let verified_fraction = if summary.total > 0 {
        summary.verified as f64 / summary.total as f64
    } else {
        0.0
    };

    json_result(&json!({
        "plan": params.plan_public_id,
        "window_days": window_days,
        "total": summary.total,
        "verified": summary.verified,
        "in_progress": summary.in_progress,
        "blocked": summary.blocked,
        "remaining": remaining,
        "verified_fraction": verified_fraction,
        "velocity_per_day": velocity_per_day,
        "verified_in_window": verified_in_window,
        "eta_days": eta_days,
        "eta_date": eta_date,
        "slope_per_day": slope_per_day,
        "regression_eta_days": regression_eta_days,
        "series": series,
    }))
}

/// `work_item_history` — the full per-item unified timeline: a chronological
/// merge of status transitions, progress notes, claim/handoff events,
/// verification evidence, and scope negotiations. Read-only. The auto-unblock
/// cascade surfaces here as an `actor_kind='system'` `blocked → ready` status
/// event on the unblocked dependent.
pub async fn tool_work_item_history(
    ctx: &SystemContext,
    params: WorkItemHistoryParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .work_item_queries
        .fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let item_id = id_of_public(pool, &params.public_id).await?;
    let limit = params.limit.unwrap_or(100);
    let timeline = queries::work_item_timeline(pool, item_id, limit)
        .await
        .map_err(map_db_err)?;

    json_result(&json!({
        "public_id": params.public_id,
        "events": timeline.len(),
        "timeline": timeline,
    }))
}

/// Markdown checkbox + a unicode status glyph for a status.
fn md_checkbox(status: &str) -> (&'static str, &'static str) {
    match status {
        "verified" => ("[x]", "✓"),
        "cancelled" => ("[-]", "✗"),
        "deferred" => ("[~]", "⏸"),
        "blocked" => ("[!]", "⛔"),
        "in_progress" | "claimed_done" | "verifying" => ("[ ]", "◐"),
        _ => ("[ ]", "○"),
    }
}

/// Org-mode TODO keyword for a status.
fn org_keyword(status: &str) -> &'static str {
    match status {
        "verified" => "DONE",
        "cancelled" => "CANCELLED",
        "deferred" => "DEFERRED",
        "blocked" => "WAITING",
        "in_progress" | "claimed_done" | "verifying" => "DOING",
        _ => "TODO",
    }
}

/// Depth-first render of the subtree rooted at `id` into `out`.
fn render_node(
    out: &mut String,
    children: &HashMap<i64, Vec<&WorkItemRow>>,
    by_id: &HashMap<i64, &WorkItemRow>,
    id: i64,
    depth: usize,
    org: bool,
) {
    let Some(row) = by_id.get(&id) else { return };
    if org {
        let stars = "*".repeat(depth + 1);
        let _ = writeln!(out, "{stars} {} {}", org_keyword(&row.status), row.title);
        let _ = writeln!(out, "  :PROPERTIES:");
        let _ = writeln!(out, "  :ID: {}", row.public_id);
        let _ = writeln!(out, "  :KIND: {}", row.kind);
        let _ = writeln!(out, "  :END:");
        if let Some(body) = row.body.as_deref().filter(|b| !b.trim().is_empty()) {
            let _ = writeln!(out, "  {}", body.replace('\n', "\n  "));
        }
    } else {
        let (checkbox, glyph) = md_checkbox(&row.status);
        let indent = "  ".repeat(depth);
        let _ = writeln!(
            out,
            "{indent}- {checkbox} {glyph} {} `{}` · {} · _{}_",
            row.title, row.public_id, row.kind, row.status
        );
    }
    if let Some(kids) = children.get(&id) {
        for k in kids {
            render_node(out, children, by_id, k.id, depth + 1, org);
        }
    }
}

/// `work_item_export` — render a plan subtree as a markdown task list or an
/// Org-mode outline (status → checkbox/keyword).
pub async fn tool_work_item_export(
    ctx: &SystemContext,
    params: WorkItemExportParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .work_item_queries
        .fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let format = params
        .format
        .as_deref()
        .unwrap_or("markdown")
        .to_ascii_lowercase();
    let org = match format.as_str() {
        "markdown" | "md" => false,
        "org" | "orgmode" | "org-mode" => true,
        other => {
            return Err(McpError::invalid_params(
                format!("unknown format '{other}'; expected 'markdown' or 'org'"),
                None,
            ));
        }
    };

    let root_id = id_of_public(pool, &params.plan_public_id).await?;
    let rows = queries::get_work_item_subtree(pool, root_id, 100_000)
        .await
        .map_err(map_db_err)?;
    if rows.is_empty() {
        return Err(McpError::invalid_params(
            format!("plan '{}' has no items", params.plan_public_id),
            None,
        ));
    }

    // Index rows + build the parent→children adjacency (iteration order is the
    // subtree query's depth/priority/id order, so siblings stay sorted).
    let by_id: HashMap<i64, &WorkItemRow> = rows.iter().map(|r| (r.id, r)).collect();
    let mut children: HashMap<i64, Vec<&WorkItemRow>> = HashMap::new();
    for r in &rows {
        if let Some(p) = r.parent_id {
            // Only thread the edge when the parent is inside the exported subtree.
            if by_id.contains_key(&p) {
                children.entry(p).or_default().push(r);
            }
        }
    }

    let mut out = String::with_capacity(rows.len() * 48);
    if !org {
        let root_title = by_id
            .get(&root_id)
            .map(|r| r.title.as_str())
            .unwrap_or("Plan");
        let _ = writeln!(out, "# {root_title}\n");
    }
    render_node(&mut out, &children, &by_id, root_id, 0, org);

    json_result(&json!({
        "plan": params.plan_public_id,
        "format": if org { "org" } else { "markdown" },
        "item_count": rows.len(),
        "content": out,
    }))
}
