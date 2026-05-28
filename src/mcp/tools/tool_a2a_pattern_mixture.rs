//! `a2a_pattern_mixture` — Mixture-of-Specialists + Summarizer pattern from
//! Yang et al. 2026 "Recursive Multi-Agent Systems" Table 1 (Mixture Style).
//!
//! Fans out the same query to N domain specialists in parallel, then
//! sends all of their outputs to a single Summarizer agent for aggregation.

#![allow(unused_imports)]

use futures::future::join_all;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use sqlx::PgPool;
use std::sync::atomic::Ordering;

use crate::a2a::client::{A2aClient, SendOptions};
use crate::a2a::types::{Part, Task};
use crate::context::SystemContext;
use crate::mcp::server::A2aPatternMixtureParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};
use crate::mcp::tools::tool_a2a_pattern_sequential::{mark_parent_completed, persist_parent_task};

pub async fn tool_a2a_pattern_mixture(
    ctx: &SystemContext,
    params: A2aPatternMixtureParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "a2a_pattern_mixture", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .a2a_pattern_mixture_invocations
        .fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    // Read-before-act (Part A): peer best practices, prepended to the
    // specialist prompt. Empty unless [a2a] inject_best_practices = true.
    let bp = crate::a2a::best_practices::retrieve_for_prompt(ctx, None, &params.message, 512).await;

    if params.specialist_agents.is_empty() {
        return Err(McpError::invalid_params(
            "specialist_agents must contain at least one agent",
            None,
        ));
    }
    if params.specialist_agents.len() > 8 {
        return Err(McpError::invalid_params(
            "specialist_agents capped at 8",
            None,
        ));
    }

    // Resolve all URLs up front so failures fail fast.
    let mut specialist_urls: Vec<(String, String)> =
        Vec::with_capacity(params.specialist_agents.len());
    for name in &params.specialist_agents {
        let url = resolve_agent_url(pool, name).await?;
        specialist_urls.push((name.clone(), url));
    }
    let summarizer_url = resolve_agent_url(pool, &params.summarizer_agent).await?;

    let parent_task_id = persist_parent_task(
        pool,
        "a2a_pattern_mixture",
        &json!({
            "pattern": "mixture",
            "specialist_agents": params.specialist_agents,
            "summarizer_agent": params.summarizer_agent,
            "message": params.message,
        }),
    )
    .await?;

    // Fan out to specialists in parallel.
    let specialist_message = format!(
        "{bp}[Role: Domain Specialist] Query:\n{}\n\nProduce your domain-specific answer.",
        params.message
    );
    let futures: Vec<_> = specialist_urls
        .iter()
        .map(|(_name, url)| {
            let url = url.clone();
            let msg = specialist_message.clone();
            let stats = ctx.stats().clone();
            async move {
                stats.a2a_peer_fanout_calls.fetch_add(1, Ordering::Relaxed);
                A2aClient::new(url)
                    .send_task_with(
                        &msg,
                        None,
                        SendOptions {
                            recursion_rounds: None,
                            parent_task_id: Some(parent_task_id),
                        },
                    )
                    .await
            }
        })
        .collect();
    let specialist_results = join_all(futures).await;

    let mut transcript: Vec<serde_json::Value> = Vec::new();
    let mut combined_outputs = String::new();
    for ((name, _url), result) in specialist_urls.iter().zip(specialist_results) {
        match result {
            Ok(task) => {
                let text = task_to_text(&task);
                transcript.push(json!({
                    "agent": name, "task_id": task.id,
                    "ok": true, "output": text,
                }));
                combined_outputs.push_str(&format!("\n## Specialist: {}\n{}\n", name, text));
            }
            Err(e) => {
                transcript.push(json!({
                    "agent": name, "ok": false, "error": e,
                }));
                combined_outputs.push_str(&format!(
                    "\n## Specialist: {} (FAILED)\n(no output: {})\n",
                    name, e
                ));
            }
        }
    }

    // Summarizer aggregates.
    let summary_prompt = format!(
        "[Role: Summarizer] Original query:\n{}\n\nSpecialist outputs:{}\n\nSynthesize a single coherent final answer that integrates the specialists' contributions.",
        params.message, combined_outputs
    );
    ctx.stats()
        .a2a_peer_fanout_calls
        .fetch_add(1, Ordering::Relaxed);
    let summary_task = A2aClient::new(summarizer_url)
        .send_task_with(
            &summary_prompt,
            None,
            SendOptions {
                recursion_rounds: None,
                parent_task_id: Some(parent_task_id),
            },
        )
        .await
        .map_err(|e| McpError::internal_error(format!("Summarizer call failed: {}", e), None))?;
    let final_text = task_to_text(&summary_task);

    // Persist the structured transcript so the CSM observer (csm_validate_run)
    // can lift this run and check it against its protocol (ADR-009). Best-effort.
    let _ = crate::csm::store::record_transcript_values(pool, parent_task_id, &transcript).await;
    mark_parent_completed(pool, parent_task_id).await?;

    // Best-practice write-back (Part A): distill the Summarizer's synthesis
    // into the shared memory graph. No-op unless [a2a] writeback_enabled.
    crate::a2a::best_practices::writeback_peer_artifact(
        ctx,
        parent_task_id,
        &params.summarizer_agent,
        "a2a_pattern_mixture:Summarizer",
        &final_text,
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
        crate::csm::registry::ProtocolId::Mixture,
        &transcript,
        ctx.config().load().a2a.protocol_interpreter,
    );
    json_result(&json!({
        "protocol": protocol_report,
        "effect_breakdown": effect_breakdown,
        "pattern": "mixture",
        "parent_task_id": parent_task_id,
        "specialists": transcript,
        "summarizer": {
            "agent": params.summarizer_agent,
            "task_id": summary_task.id,
        },
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
