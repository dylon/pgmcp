//! Read/compute analysis tools for the tracker: completion roll-up and
//! (re)prioritization. Both are pure reads/recomputes over existing rows — no
//! lifecycle transitions happen here.

#![allow(unused_imports)]

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::{Value, json};

use crate::context::SystemContext;
use crate::db::queries::{self, WorkItemRow};
use crate::mcp::server::{
    WorkItemCompletionParams, WorkItemReprioritizeParams, WorkItemSearchParams,
};
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};
use crate::mcp::tools::work_items::crud::{
    id_of_public, map_db_err, resolve_existing_project_id_param,
};

/// Round a fraction (0.0–1.0) to a 0.0–100.0 percentage with one decimal.
fn pct(frac: f64) -> f64 {
    (frac * 1000.0).round() / 10.0
}

/// Compact summary of an item for a ranked plan (omits the heavy fields).
fn summarize(r: &WorkItemRow) -> Value {
    json!({
        "public_id": r.public_id,
        "title": r.title,
        "kind": r.kind,
        "status": r.status,
        "priority": r.priority,
        "computed_score": r.computed_score,
    })
}

/// `work_item_completion` — weighted completion roll-up of a subtree. Returns
/// BOTH the trustworthy `verified_*` numbers (only evidence-verified leaves)
/// and the advisory `claimed_*` numbers (incl. agent self-reports), kept
/// distinct on purpose.
pub async fn tool_work_item_completion(
    ctx: &SystemContext,
    params: WorkItemCompletionParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .work_item_queries
        .fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let id = id_of_public(pool, &params.public_id).await?;
    let r = queries::compute_rollup(pool, id)
        .await
        .map_err(map_db_err)?;
    json_result(&json!({
        "public_id": params.public_id,
        "verified_fraction": r.verified_fraction,
        "verified_pct": pct(r.verified_fraction),
        "claimed_fraction": r.claimed_fraction,
        "claimed_pct": pct(r.claimed_fraction),
        "leaf_count": r.leaf_count,
        "verified_leaves": r.verified_leaves,
        "claimed_leaves": r.claimed_leaves,
        "note": "verified_* counts only evidence-verified leaves; claimed_* additionally counts agent-reported claimed_done (advisory).",
    }))
}

/// `work_item_reprioritize` — recompute `computed_score` for active items
/// (recency × priority × dependency-unblock) and return a now/next/later plan
/// of the top items by score.
pub async fn tool_work_item_reprioritize(
    ctx: &SystemContext,
    params: WorkItemReprioritizeParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .work_item_reprioritizations
        .fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let project_id = resolve_existing_project_id_param(pool, params.project.as_deref()).await?;
    let half_life = params.half_life_days.unwrap_or(14.0);
    let limit = params.limit.unwrap_or(30);
    let ranked = queries::reprioritize_work_items(pool, project_id, half_life, limit)
        .await
        .map_err(map_db_err)?;

    // Bucket the ranked top into a now/next/later work plan.
    let now_n = ranked.len().min(5);
    let next_n = ranked.len().saturating_sub(now_n).min(10);
    let now: Vec<Value> = ranked[..now_n].iter().map(summarize).collect();
    let next: Vec<Value> = ranked[now_n..now_n + next_n]
        .iter()
        .map(summarize)
        .collect();
    let later: Vec<Value> = ranked[now_n + next_n..].iter().map(summarize).collect();

    json_result(&json!({
        "shown": ranked.len(),
        "half_life_days": half_life,
        "now": now,
        "next": next,
        "later": later,
        "note": "All active items in scope were rescored; this shows the top by computed_score.",
    }))
}

/// `work_item_search` — semantic (cosine) search over the backlog by meaning.
/// Embeds the query and returns the nearest items (by `work_items.embedding`)
/// with their similarity. Only items embedded on write are matchable.
pub async fn tool_work_item_search(
    ctx: &SystemContext,
    params: WorkItemSearchParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .work_item_queries
        .fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;
    let q = params.query.trim();
    if q.is_empty() {
        return Err(McpError::invalid_params("query must be non-empty", None));
    }
    let project = params
        .project
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let qvec = ctx
        .embed()
        .embed_query(q)
        .await
        .map_err(|e| McpError::internal_error(format!("embed failed: {e}"), None))?;
    if qvec.len() != 1024 {
        return Err(McpError::internal_error(
            format!(
                "query embedding dimension mismatch: got {}, expected 1024",
                qvec.len()
            ),
            None,
        ));
    }
    let project_id = resolve_existing_project_id_param(pool, params.project.as_deref()).await?;
    let limit = params.limit.unwrap_or(10).clamp(1, 100);
    let hits = queries::search_work_items(pool, pgvector::Vector::from(qvec), project_id, limit)
        .await
        .map_err(map_db_err)?;
    json_result(&json!({ "query": q, "project": project, "limit": limit, "hits": hits }))
}
