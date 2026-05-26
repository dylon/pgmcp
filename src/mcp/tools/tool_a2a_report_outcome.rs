//! `a2a_report_outcome` — explicit best-practice capture (Part A).
//!
//! Any MCP client (Claude Code, Codex CLI, a peer pgmcp daemon) calls this
//! to record that an approach worked / failed for a kind of task. The
//! report lands in `agent_outcomes` AND a mirrored `memory_observation`
//! via [`crate::a2a::best_practices::record_outcome`], so it participates
//! in cross-agent reflection (A4) and is retrievable by future agents
//! (A3). LLM-free: works with `[memory] backend = "disabled"`.

#![allow(unused_imports)]

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::a2a::best_practices::{Outcome, OutcomeReport, record_outcome};
use crate::context::SystemContext;
use crate::mcp::server::A2aReportOutcomeParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

pub async fn tool_a2a_report_outcome(
    ctx: &SystemContext,
    params: A2aReportOutcomeParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "a2a_report_outcome", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let outcome = Outcome::parse(&params.outcome).ok_or_else(|| {
        McpError::invalid_params(
            format!(
                "unknown outcome '{}'; expected one of: worked, failed, mixed, prefer, avoid, superseded_by_peer",
                params.outcome
            ),
            None,
        )
    })?;
    if params.task_kind.trim().is_empty() || params.approach.trim().is_empty() {
        return Err(McpError::invalid_params(
            "task_kind and approach must be non-empty",
            None,
        ));
    }

    // Attribution: the #[tool] method fills agent_id from the MCP client
    // name; the CLI / test path may leave it unset.
    let agent_id = params
        .agent_id
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "unknown-agent".to_string());

    let report = OutcomeReport {
        agent_id,
        project_id: params.project_id,
        task_kind: params.task_kind.clone(),
        approach: params.approach.clone(),
        outcome,
        confidence: params.confidence.unwrap_or(0.6),
        evidence: params.evidence.clone(),
        parent_task_id: None,
        // Explicit operator/agent reports are strong signal → procedural tier.
        tier: "procedural",
    };

    let recorded = record_outcome(pool, &report)
        .await
        .map_err(|e| McpError::internal_error(format!("record_outcome failed: {e}"), None))?;

    ctx.stats()
        .a2a_outcomes_recorded
        .fetch_add(1, Ordering::Relaxed);

    json_result(&json!({
        "recorded": true,
        "outcome_id": recorded.outcome_id,
        "observation_id": recorded.observation_id,
        "approach_entity_id": recorded.approach_entity_id,
        "agent_id": report.agent_id,
        "outcome": outcome.as_db_str(),
    }))
}
