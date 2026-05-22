//! `a2a_pattern_deliberation` — Reflector ↔ Tool-Caller iterative loop
//! from Yang et al. 2026 "Recursive Multi-Agent Systems" Table 1
//! (Deliberation Style).
//!
//! The Reflector inspects the query and proposes a sub-task; the
//! Tool-Caller acts on it; the Reflector reviews and either iterates or
//! signals convergence. Terminates on convergence marker or `max_rounds`.

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use sqlx::PgPool;
use std::sync::atomic::Ordering;

use crate::a2a::client::{A2aClient, SendOptions};
use crate::a2a::types::{Part, Task};
use crate::context::SystemContext;
use crate::mcp::server::A2aPatternDeliberationParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};
use crate::mcp::tools::tool_a2a_pattern_sequential::{mark_parent_completed, persist_parent_task};

pub async fn tool_a2a_pattern_deliberation(
    ctx: &SystemContext,
    params: A2aPatternDeliberationParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "a2a_pattern_deliberation", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .a2a_pattern_deliberation_invocations
        .fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let reflector_url = resolve_agent_url(pool, &params.reflector_agent).await?;
    let tool_caller_url = resolve_agent_url(pool, &params.tool_caller_agent).await?;

    let parent_task_id = persist_parent_task(
        pool,
        "a2a_pattern_deliberation",
        &json!({
            "pattern": "deliberation",
            "reflector_agent": params.reflector_agent,
            "tool_caller_agent": params.tool_caller_agent,
            "message": params.message,
        }),
    )
    .await?;
    let parent_opts = SendOptions {
        recursion_rounds: None,
        parent_task_id: Some(parent_task_id),
    };

    let max_rounds = params.max_rounds.unwrap_or(3).clamp(1, 10);
    let mut transcript: Vec<serde_json::Value> = Vec::new();
    let mut state_text = String::new();
    let mut converged = false;
    let mut final_answer = String::new();

    for round in 0..max_rounds {
        // Reflector turn.
        let reflector_prompt = if round == 0 {
            format!(
                "[Role: Reflector — round {}] Query:\n{}\n\nPropose a single concrete sub-task or refinement, or return literal 'CONVERGED' followed by the final answer.",
                round, params.message
            )
        } else {
            format!(
                "[Role: Reflector — round {}] Query:\n{}\n\nState so far:\n{}\n\nPropose a single concrete sub-task or refinement, or return literal 'CONVERGED' followed by the final answer.",
                round, params.message, state_text
            )
        };
        ctx.stats()
            .a2a_peer_fanout_calls
            .fetch_add(1, Ordering::Relaxed);
        let reflector_task = A2aClient::new(reflector_url.clone())
            .send_task_with(&reflector_prompt, None, parent_opts)
            .await
            .map_err(|e| McpError::internal_error(format!("Reflector failed: {}", e), None))?;
        let reflector_text = task_to_text(&reflector_task);
        transcript.push(json!({
            "round": round, "role": "Reflector",
            "agent": params.reflector_agent, "task_id": reflector_task.id,
            "output": reflector_text,
        }));
        if let Some(rest) = reflector_text.find("CONVERGED") {
            converged = true;
            final_answer = reflector_text[rest + "CONVERGED".len()..]
                .trim()
                .to_string();
            break;
        }

        // Tool-Caller turn.
        let tool_prompt = format!(
            "[Role: Tool-Caller — round {}] Original query:\n{}\n\nReflector's directive:\n{}\n\nExecute the directive (call tools, gather information, produce an artifact).",
            round, params.message, reflector_text
        );
        ctx.stats()
            .a2a_peer_fanout_calls
            .fetch_add(1, Ordering::Relaxed);
        let tool_task = A2aClient::new(tool_caller_url.clone())
            .send_task_with(&tool_prompt, None, parent_opts)
            .await
            .map_err(|e| McpError::internal_error(format!("Tool-Caller failed: {}", e), None))?;
        let tool_text = task_to_text(&tool_task);
        transcript.push(json!({
            "round": round, "role": "Tool-Caller",
            "agent": params.tool_caller_agent, "task_id": tool_task.id,
            "output": tool_text,
        }));
        state_text.push_str(&format!(
            "\nRound {} Reflector: {}\nRound {} Tool-Caller: {}\n",
            round, reflector_text, round, tool_text
        ));
        final_answer = tool_text;
    }

    mark_parent_completed(pool, parent_task_id).await?;

    json_result(&json!({
        "pattern": "deliberation",
        "parent_task_id": parent_task_id,
        "rounds_executed": transcript.iter().filter(|e| e["role"] == "Tool-Caller").count(),
        "converged": converged,
        "transcript": transcript,
        "final_answer": final_answer,
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
