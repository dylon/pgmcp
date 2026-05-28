//! `a2a_pattern_distillation` — Expert → Learner pair from Yang et al.
//! 2026 "Recursive Multi-Agent Systems" Table 1 (Distillation Style).
//!
//! Sends the query to an Expert peer, captures its answer, then asks a
//! smaller / faster Learner peer to produce its own answer conditioned on
//! the Expert's reasoning. Returns both answers so the caller can compare.

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use sqlx::PgPool;
use std::sync::atomic::Ordering;
use std::time::Instant;

use crate::a2a::client::{A2aClient, SendOptions};
use crate::a2a::types::{Part, Task};
use crate::context::SystemContext;
use crate::mcp::server::A2aPatternDistillationParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};
use crate::mcp::tools::tool_a2a_pattern_sequential::{mark_parent_completed, persist_parent_task};

pub async fn tool_a2a_pattern_distillation(
    ctx: &SystemContext,
    params: A2aPatternDistillationParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "a2a_pattern_distillation", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .a2a_pattern_distillation_invocations
        .fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    // Read-before-act (Part A): peer best practices, prepended to the
    // Expert prompt. Empty unless [a2a] inject_best_practices = true.
    let bp = crate::a2a::best_practices::retrieve_for_prompt(ctx, None, &params.message, 512).await;

    let expert_url = resolve_agent_url(pool, &params.expert_agent).await?;
    let learner_url = resolve_agent_url(pool, &params.learner_agent).await?;

    let parent_task_id = persist_parent_task(
        pool,
        "a2a_pattern_distillation",
        &json!({
            "pattern": "distillation",
            "expert_agent": params.expert_agent,
            "learner_agent": params.learner_agent,
            "message": params.message,
        }),
    )
    .await?;
    let parent_opts = SendOptions {
        recursion_rounds: None,
        parent_task_id: Some(parent_task_id),
    };

    // 1. Expert answer.
    let expert_prompt = format!(
        "{bp}[Role: Expert] Query:\n{}\n\nProduce a thorough, high-quality answer. Show your reasoning explicitly.",
        params.message
    );
    let t_expert_start = Instant::now();
    ctx.stats()
        .a2a_peer_fanout_calls
        .fetch_add(1, Ordering::Relaxed);
    let expert_task = A2aClient::new(expert_url)
        .send_task_with(&expert_prompt, None, parent_opts)
        .await
        .map_err(|e| McpError::internal_error(format!("Expert call failed: {}", e), None))?;
    let expert_latency_ms = t_expert_start.elapsed().as_millis() as u64;
    let expert_text = task_to_text(&expert_task);

    // 2. Learner answer (conditioned on Expert's reasoning).
    let learner_prompt = format!(
        "[Role: Learner] Query:\n{}\n\nExpert's answer & reasoning:\n{}\n\nProduce your own answer — concise, distilled, and efficient. Match the Expert's correctness where possible.",
        params.message, expert_text
    );
    let t_learner_start = Instant::now();
    ctx.stats()
        .a2a_peer_fanout_calls
        .fetch_add(1, Ordering::Relaxed);
    let learner_task = A2aClient::new(learner_url)
        .send_task_with(&learner_prompt, None, parent_opts)
        .await
        .map_err(|e| McpError::internal_error(format!("Learner call failed: {}", e), None))?;
    let learner_latency_ms = t_learner_start.elapsed().as_millis() as u64;
    let learner_text = task_to_text(&learner_task);

    // CSM observer transcript (ADR-009): Expert then Learner. Best-effort.
    let distill_transcript = vec![
        serde_json::json!({"round": 0, "role": "Expert", "output": expert_text.clone()}),
        serde_json::json!({"round": 0, "role": "Learner", "output": learner_text.clone()}),
    ];
    let _ = crate::csm::store::record_transcript_values(pool, parent_task_id, &distill_transcript)
        .await;
    mark_parent_completed(pool, parent_task_id).await?;

    // Best-practice write-back (Part A): distill the Learner's distilled
    // answer into the shared memory graph. No-op unless [a2a] writeback_enabled.
    crate::a2a::best_practices::writeback_peer_artifact(
        ctx,
        parent_task_id,
        &params.learner_agent,
        "a2a_pattern_distillation:Learner",
        &learner_text,
    )
    .await;

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

    let protocol_report = crate::csm::driver::driver_report(
        crate::csm::registry::ProtocolId::Distillation,
        &distill_transcript,
        ctx.config().load().a2a.protocol_interpreter,
    );
    json_result(&json!({
        "protocol": protocol_report,
        "effect_breakdown": effect_breakdown,
        "pattern": "distillation",
        "parent_task_id": parent_task_id,
        "next": format!("Feed the conformance learner: csm_validate_run(task_id='{parent_task_id}')"),
        "expert": {
            "agent": params.expert_agent,
            "task_id": expert_task.id,
            "latency_ms": expert_latency_ms,
            "output": expert_text,
            "output_chars": expert_text.len(),
        },
        "learner": {
            "agent": params.learner_agent,
            "task_id": learner_task.id,
            "latency_ms": learner_latency_ms,
            "output": learner_text,
            "output_chars": learner_text.len(),
        },
        "compression_ratio":
            if expert_text.is_empty() { 0.0 }
            else { learner_text.len() as f64 / expert_text.len() as f64 },
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
