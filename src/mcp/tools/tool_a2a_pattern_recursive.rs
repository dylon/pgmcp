//! `a2a_pattern_recursive` — RLM-style recursive decomposition (Part B).
//!
//! Treats a corpus/file as an external environment, decomposes it into
//! snippets, recursively sub-calls a peer LM over each (small context),
//! and stitches the partials — the full context is never inlined. See
//! `crate::a2a::rlm` for the engine.

#![allow(unused_imports)]

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use sqlx::PgPool;

use crate::a2a::rlm::{RlmEnvironment, RlmFrame, own_a2a_url, run_rlm};
use crate::context::SystemContext;
use crate::mcp::server::A2aPatternRecursiveParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};
use crate::mcp::tools::tool_a2a_pattern_sequential::{mark_parent_completed, persist_parent_task};

pub async fn tool_a2a_pattern_recursive(
    ctx: &SystemContext,
    params: A2aPatternRecursiveParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "a2a_pattern_recursive", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .a2a_rlm_invocations
        .fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let env = RlmEnvironment::from_json(&params.environment)
        .map_err(|e| McpError::invalid_params(e, None))?;
    let sub_url = resolve_agent_url(pool, &params.sub_agent).await?;
    let reduce_url = match &params.reduce_agent {
        Some(a) => Some(resolve_agent_url(pool, a).await?),
        None => None,
    };

    // Depth/budget default from [a2a.rlm]; per-call params override (clamped
    // to the hard caps inside RlmFrame::new_root). The default depth keeps
    // stock installs at the depth-1 (B1) sequential behavior.
    let (def_depth, def_budget) = {
        let acfg = ctx.config().load();
        (acfg.a2a.rlm.max_depth, acfg.a2a.rlm.max_budget)
    };

    let parent_task_id = persist_parent_task(
        pool,
        "a2a_pattern_recursive",
        &json!({
            "pattern": "recursive",
            "environment": params.environment,
            "sub_agent": params.sub_agent,
            "query": params.query,
        }),
        None,
    )
    .await?;

    // Root frame: the daemon's own A2A URL anchors self-recursion; the LM
    // peer URLs are resolved here once and carried down the tree.
    let frame = RlmFrame::new_root(
        own_a2a_url(ctx),
        params.environment.clone(),
        params.query.clone(),
        sub_url,
        reduce_url,
        params.max_chunks.unwrap_or(8).clamp(1, 64),
        params.concurrency.unwrap_or(4),
        params.strategy.clone(),
        params.verify.unwrap_or(false),
        params.rlm_depth.unwrap_or(def_depth),
        params.rlm_budget.unwrap_or(def_budget),
        parent_task_id,
    );

    let outcome = run_rlm(ctx, &env, &frame, parent_task_id).await?;

    ctx.stats()
        .a2a_rlm_subcalls
        .fetch_add(outcome.subcalls as u64, Ordering::Relaxed);

    // B3: persist the trajectory (typed steps + encoded f64 series for the
    // MSM trajectory index). Best-effort — a persistence hiccup must not
    // sink the answer we already computed.
    let trajectory_id = match crate::a2a::rlm::persist_trajectory(
        pool,
        parent_task_id,
        None,
        &params.environment,
        &params.query,
        &outcome,
    )
    .await
    {
        Ok(id) => Some(id),
        Err(e) => {
            tracing::error!(error = %e, "RLM trajectory persistence failed (non-fatal)");
            None
        }
    };

    mark_parent_completed(pool, parent_task_id).await?;

    // Best-practice write-back (Part A): record the RLM outcome. No-op
    // unless [a2a] writeback_enabled.
    crate::a2a::best_practices::writeback_peer_artifact(
        ctx,
        parent_task_id,
        &params.sub_agent,
        "a2a_pattern_recursive",
        &outcome.final_answer,
    )
    .await;

    let trajectory: Vec<serde_json::Value> = outcome
        .steps
        .iter()
        .map(|s| {
            json!({
                "ord": s.ord,
                "kind": s.kind.as_str(),
                "depth": s.depth,
                "latency_ms": s.latency_ms,
                "est_tokens": s.est_tokens,
                "success": s.success,
            })
        })
        .collect();

    json_result(&json!({
        "pattern": "recursive",
        "parent_task_id": parent_task_id,
        "next": format!("Feed the conformance learner: csm_validate_run(task_id='{parent_task_id}')"),
        "trajectory_id": trajectory_id,
        "strategy": outcome.strategy,
        "chunks": outcome.chunks,
        "subcalls": outcome.subcalls,
        "verified": outcome.verified,
        "trajectory": trajectory,
        "final_answer": outcome.final_answer,
    }))
}

async fn resolve_agent_url(pool: &PgPool, name: &str) -> Result<String, McpError> {
    let row: Option<(String,)> =
        sqlx::query_as::<_, (String,)>("SELECT url FROM a2a_agents WHERE name = $1")
            .bind(name)
            .fetch_optional(pool)
            .await
            .map_err(|e| McpError::internal_error(format!("Agent lookup failed: {}", e), None))?;
    row.map(|(u,)| u)
        .ok_or_else(|| McpError::internal_error(format!("Agent not registered: {}", name), None))
}
