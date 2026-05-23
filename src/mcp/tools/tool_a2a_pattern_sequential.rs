//! `a2a_pattern_sequential` — Planner → Critic → Solver collaboration
//! pattern from Yang et al. 2026 "Recursive Multi-Agent Systems" Table 1
//! (Sequential Style).
//!
//! Threads three peer A2A agents in order: the Planner produces a plan,
//! the Critic reviews it, the Solver produces the final answer
//! conditioned on both. Each round's output becomes conditioning context
//! for the next call. When `recursion_rounds > 1`, the trio runs N times
//! with the previous round's Solver output threaded into the next round's
//! Planner prompt for iterative refinement.

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use sqlx::PgPool;
use std::sync::atomic::Ordering;
use uuid::Uuid;

use crate::a2a::client::{A2aClient, SendOptions};
use crate::a2a::types::{Part, Task};
use crate::context::SystemContext;
use crate::mcp::server::A2aPatternSequentialParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

pub async fn tool_a2a_pattern_sequential(
    ctx: &SystemContext,
    params: A2aPatternSequentialParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "a2a_pattern_sequential", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .a2a_pattern_sequential_invocations
        .fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let planner_url = resolve_agent_url(pool, &params.planner_agent).await?;
    let critic_url = resolve_agent_url(pool, &params.critic_agent).await?;
    let solver_url = resolve_agent_url(pool, &params.solver_agent).await?;

    let parent_task_id = persist_parent_task(
        pool,
        "a2a_pattern_sequential",
        &json!({
            "pattern": "sequential",
            "planner_agent": params.planner_agent,
            "critic_agent": params.critic_agent,
            "solver_agent": params.solver_agent,
            "message": params.message,
        }),
    )
    .await?;

    let rounds = params.recursion_rounds.unwrap_or(1).clamp(1, 5);
    let mut transcript: Vec<serde_json::Value> = Vec::new();
    let mut final_text = String::new();
    let mut prev_solver_output: Option<String> = None;

    for round in 0..rounds {
        // 1. Planner — refines based on previous round's solver output (if any).
        let plan_prompt = match &prev_solver_output {
            None => format!(
                "[Role: Planner] Original query:\n{}\n\nProduce a step-by-step plan to solve the query.",
                params.message
            ),
            Some(prev) => format!(
                "[Role: Planner — refinement round {}] Original query:\n{}\n\nPrevious round's Solver output:\n{}\n\nProduce a refined step-by-step plan that addresses gaps in the previous answer.",
                round, params.message, prev
            ),
        };
        let plan_task = call_peer(&planner_url, &plan_prompt, ctx, parent_task_id).await?;
        let plan_text = task_to_text(&plan_task);
        transcript.push(json!({
            "round": round, "role": "Planner",
            "agent": params.planner_agent, "task_id": plan_task.id,
            "output": plan_text,
        }));

        // 2. Critic
        let critic_prompt = format!(
            "[Role: Critic] Original query:\n{}\n\nPlanner's plan:\n{}\n\nCritique the plan. Identify gaps, risks, and improvements.",
            params.message, plan_text
        );
        let critic_task = call_peer(&critic_url, &critic_prompt, ctx, parent_task_id).await?;
        let critique_text = task_to_text(&critic_task);
        transcript.push(json!({
            "round": round, "role": "Critic",
            "agent": params.critic_agent, "task_id": critic_task.id,
            "output": critique_text,
        }));

        // 3. Solver
        let solver_prompt = format!(
            "[Role: Solver] Original query:\n{}\n\nPlanner's plan:\n{}\n\nCritic's notes:\n{}\n\nProduce the final answer.",
            params.message, plan_text, critique_text
        );
        let solver_task = call_peer(&solver_url, &solver_prompt, ctx, parent_task_id).await?;
        let solver_text = task_to_text(&solver_task);
        transcript.push(json!({
            "round": round, "role": "Solver",
            "agent": params.solver_agent, "task_id": solver_task.id,
            "output": solver_text,
        }));
        prev_solver_output = Some(solver_text.clone());
        final_text = solver_text;
    }

    mark_parent_completed(pool, parent_task_id).await?;

    // Shadow-ASR channel (Phase D2b): workspace-wide effect distribution.
    let effect_breakdown: Vec<serde_json::Value> = (async {
        let Some(pool) = ctx.db().pool() else {
            return Vec::new();
        };
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT se.effect, COUNT(*)::int8
             FROM symbol_effects se
             GROUP BY se.effect
             ORDER BY se.effect",
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        rows.into_iter()
            .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
            .collect()
    })
    .await;

    json_result(&json!({
        "effect_breakdown": effect_breakdown,
        "pattern": "sequential",
        "parent_task_id": parent_task_id,
        "rounds": rounds,
        "transcript": transcript,
        "final_answer": final_text,
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

async fn call_peer(
    url: &str,
    text: &str,
    ctx: &SystemContext,
    parent_task_id: Uuid,
) -> Result<Task, McpError> {
    ctx.stats()
        .a2a_peer_fanout_calls
        .fetch_add(1, Ordering::Relaxed);
    let opts = SendOptions {
        recursion_rounds: None,
        parent_task_id: Some(parent_task_id),
    };
    A2aClient::new(url.to_string())
        .send_task_with(text, None, opts)
        .await
        .map_err(|e| McpError::internal_error(format!("A2A peer call failed: {}", e), None))
}

fn task_to_text(task: &Task) -> String {
    let mut out = String::new();
    for art in &task.artifacts {
        for p in &art.parts {
            if let Part::Text { text, .. } = p {
                out.push_str(text);
                out.push('\n');
            }
        }
    }
    out
}

/// Persist a parent orchestration row in `a2a_tasks`. Children of this
/// pattern (the peer-side tasks created by `A2aClient::send_task_with`
/// when the peer is also a pgmcp instance) will carry this UUID as their
/// `parent_task_id`, enabling cross-instance correlation.
pub(crate) async fn persist_parent_task(
    pool: &PgPool,
    skill_id: &str,
    metadata: &serde_json::Value,
) -> Result<Uuid, McpError> {
    let parent_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO a2a_tasks
            (id, skill_id, status, metadata, recursion_rounds, current_round)
         VALUES ($1, $2, 'working', $3, 1, 0)",
    )
    .bind(parent_id)
    .bind(skill_id)
    .bind(metadata)
    .execute(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Persist parent task failed: {}", e), None))?;
    Ok(parent_id)
}

/// Mark the parent task as completed once all child peer calls have
/// returned. Failures here are non-fatal — the orchestration result is
/// returned to the caller either way.
pub(crate) async fn mark_parent_completed(pool: &PgPool, parent_id: Uuid) -> Result<(), McpError> {
    sqlx::query(
        "UPDATE a2a_tasks
            SET status = 'completed',
                completed_at = NOW(),
                updated_at = NOW(),
                current_round = 1
          WHERE id = $1",
    )
    .bind(parent_id)
    .execute(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Mark parent completed failed: {}", e), None))?;
    Ok(())
}
