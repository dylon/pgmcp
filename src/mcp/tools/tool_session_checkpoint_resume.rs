//! `session_checkpoint_resume` — replay a paused Crucible session's trace to
//! recover its position and return the orchestrator's next step (ADR-009 RESUME).
//!
//! ## Boundary
//!
//! REPLAY + VALIDATE only. pgmcp reads its OWN `orchestration_sessions` +
//! `csm_run_traces` tables, rebuilds the projected `Network` from the stored
//! `GlobalType`, replays the recorded trace, re-claims the work-item lease, and
//! returns the next `(peer, request, response)` step as JSON. It NEVER runs a
//! shell or touches the user's files — the orchestrator (pi) executes the step.
//!
//! ## The trace IS the position
//!
//! 1. Load the checkpoint; rebuild `Network::build(protocol_name, global_type)`.
//! 2. Reconstruct the executed trace: the flushed `csm_run_traces.events` for the
//!    session's `task_id`, plus any unflushed `transcript` tail on the row.
//! 3. `replay_to_states` recovers each role's `LocalState`. A `Step` error means
//!    the recorded trace is corrupt — RESUME refuses loudly (logs at `error!`).
//!    `Incomplete` is *not* an error here (a paused prefix is legitimately
//!    non-terminal); `replay_to_states` never returns it.
//! 4. `next_step_from(orchestrator_state)` yields the next step — or, when the
//!    orchestrator faces the Critic `Choice`, a `{await:"critic_verdict"}` so the
//!    caller drives the `pass`/`revise` branch at runtime.
//! 5. Re-claim the `work_item_root` lease, set status `running`, return the step.
//!
//! `fork:true` first copies the checkpoint into a fresh child session
//! (`parent_session_id` set, new `session_key`) and resumes the fork.

use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::{Value, json};
use sqlx::PgPool;
use tracing::error;
use uuid::Uuid;

use crate::context::SystemContext;
use crate::csm::conformance::{Event, replay_to_states};
use crate::csm::driver::ProtocolDriver;
use crate::csm::machine::Network;
use crate::csm::mpst::global::GlobalType;
use crate::csm::role::Role;
use crate::csm::session_store::{
    SessionCheckpoint, SessionStatus, fork_checkpoint, load_checkpoint, mark_status,
};
use crate::db::queries::get_work_item_by_public_id;
use crate::mcp::server::SessionCheckpointResumeParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

/// Default work-item lease window when the caller omits `lease_secs`.
const DEFAULT_LEASE_SECS: i64 = 900;

/// Read the flushed orchestrator-side events recorded under `task_id` in
/// `csm_run_traces` (the most recent row), as a [`Vec<Event>`]. Empty if none.
async fn flushed_events(pool: &PgPool, task_id: Uuid) -> Result<Vec<Event>, sqlx::Error> {
    let row: Option<(Value,)> = sqlx::query_as(
        "SELECT events FROM csm_run_traces WHERE task_id = $1 ORDER BY id DESC LIMIT 1",
    )
    .bind(task_id)
    .fetch_optional(pool)
    .await?;
    Ok(row
        .and_then(|(events,)| serde_json::from_value(events).ok())
        .unwrap_or_default())
}

/// Resolve which checkpoint to resume: the original, or (when `fork`) a freshly
/// forked child. Returns the row that is being resumed (and which `session_key`
/// the status transitions apply to).
async fn resolve_target(
    pool: &PgPool,
    params: &SessionCheckpointResumeParams,
) -> Result<SessionCheckpoint, McpError> {
    if params.fork {
        let new_key = params
            .new_session_key
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                McpError::invalid_params("fork=true requires a non-empty new_session_key", None)
            })?;
        // Forking a non-existent parent is a hard error (nothing to copy).
        fork_checkpoint(pool, &params.session_key, new_key)
            .await
            .map_err(|e| McpError::internal_error(format!("fork failed: {e}"), None))?
            .ok_or_else(|| {
                McpError::invalid_params(
                    format!("no session '{}' to fork", params.session_key),
                    None,
                )
            })
    } else {
        load_checkpoint(pool, &params.session_key)
            .await
            .map_err(|e| McpError::internal_error(format!("checkpoint load failed: {e}"), None))?
            .ok_or_else(|| {
                McpError::invalid_params(format!("no session '{}'", params.session_key), None)
            })
    }
}

pub async fn tool_session_checkpoint_resume(
    ctx: &SystemContext,
    params: SessionCheckpointResumeParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let row = resolve_target(pool, &params).await?;
    let resumed_key = row.session_key.clone();

    // 1. Rebuild the projected network from the stored GlobalType.
    let g: GlobalType = serde_json::from_value(row.global_type.clone()).map_err(|e| {
        // A corrupt stored global type is unrecoverable — refuse loudly.
        error!(session_key = %resumed_key, error = %e, "resume: stored global_type is not a GlobalType");
        McpError::internal_error(format!("stored global_type is corrupt: {e}"), None)
    })?;
    let net = Network::build(row.protocol_name.clone(), &g).map_err(|e| {
        error!(session_key = %resumed_key, error = %e.message(), "resume: network rebuild failed");
        McpError::internal_error(format!("network rebuild failed: {}", e.message()), None)
    })?;
    let orchestrator = Role::new(row.orchestrator_role.clone());

    // 2. Reconstruct the executed trace: flushed csm_run_traces.events + the
    //    unflushed transcript tail. (If both are present the flushed copy is the
    //    durable prefix and the transcript holds steps taken since the last flush;
    //    a paused session flushes its whole transcript, so typically one is empty.)
    let mut trace: Vec<Event> = match row.task_id {
        Some(tid) => flushed_events(pool, tid)
            .await
            .map_err(|e| McpError::internal_error(format!("trace read failed: {e}"), None))?,
        None => Vec::new(),
    };
    let unflushed = row.transcript_events();
    // Append only the unflushed suffix not already covered by the flushed prefix.
    if unflushed.len() > trace.len() {
        trace.extend_from_slice(&unflushed[trace.len()..]);
    } else if trace.is_empty() {
        trace = unflushed;
    }

    // 3. Replay → position. A Step error ⇒ corrupt trace ⇒ refuse loudly.
    let states = replay_to_states(&net, &trace).map_err(|e| {
        error!(
            session_key = %resumed_key, error = %e.message(),
            "resume: recorded trace does not replay (corrupt) — refusing"
        );
        McpError::internal_error(
            format!("recorded trace is not a conformant prefix: {}", e.message()),
            None,
        )
    })?;
    let orch_state = *states.get(&orchestrator).ok_or_else(|| {
        McpError::internal_error(
            format!("orchestrator role '{orchestrator}' not in the network"),
            None,
        )
    })?;

    // Rehydrate the context-tape WORKING SET for the recovered position. The
    // orchestration cursor `row.cursor` recovered above (the protocol position)
    // is the SAME `state_cursor` the paging engine keyed its working set under —
    // "the trace IS the position" — so loading by (resumed_key, cursor)
    // reconstructs the resident set the session paused with. `load_working_set`
    // replays the persisted logical metadata deterministically (FIFO order by
    // `last_access_ord`), so the rehydrated residency is bit-identical to the
    // pre-pause snapshot. A fresh fork has no working set of its own yet (the
    // child table is keyed by session_key and is not copied by fork_checkpoint),
    // so it correctly rehydrates to zero resident pages. A DB fault is
    // ADR-021 error!-grade.
    let working_set = crate::tape::store::load_working_set(pool, &resumed_key, row.cursor)
        .await
        .map_err(|e| {
            error!(
                session_key = %resumed_key, cursor = row.cursor, error = %e,
                "resume: working-set metadata load failed"
            );
            McpError::internal_error(format!("working-set load failed: {e}"), None)
        })?;
    // Actually RECONSTRUCT the in-RAM `TapeStore` from the persisted scratch-page
    // bytes (the v53 `content` column), not just count the metadata. Scratch pages
    // (accumulator / REPL output) have no corpus source, so their bytes must be
    // rebuilt here; corpus/observation/summary pages are re-fetched lazily on first
    // access. Without this the resumed session's pages would be "resident" in the
    // metadata but absent from RAM — the inert round-trip the review flagged. A DB
    // fault is ADR-021 error!-grade.
    let rehydrated_scratch_pages = crate::tape::store::rehydrate_store_from_pages(
        pool,
        ctx.tape_registry(),
        &resumed_key,
        row.cursor,
    )
    .await
    .map_err(|e| {
        error!(
            session_key = %resumed_key, cursor = row.cursor, error = %e,
            "resume: TapeStore rehydrate failed"
        );
        McpError::internal_error(format!("TapeStore rehydrate failed: {e}"), None)
    })?;
    let working_set_summary = json!({
        "resident_pages": working_set.pages.len(),
        "resident_tokens": working_set.resident_tokens,
        "budget_tokens": working_set.budget_tokens,
        "policy": working_set.policy.as_str(),
        "logical_clock": working_set.clock,
        "rehydrated_scratch_pages": rehydrated_scratch_pages,
    });

    // 4. Next step — or the Critic-verdict await at a Choice.
    let machine = net
        .machine(&orchestrator)
        .expect("orchestrator machine (role present in network)");
    let (next_step, next_choice, done) =
        match ProtocolDriver::next_step_from(&net, &orchestrator, orch_state) {
            Some(step) => {
                let peer_role = step.peer.as_str();
                let agent = row.role_peer.get(peer_role).cloned();
                (
                    Some(json!({
                        "peer_role": peer_role,
                        "agent": agent,
                        "request": step.request.name,
                        "response": step.response.name,
                    })),
                    None,
                    false,
                )
            }
            None if machine.is_terminal(orch_state) => {
                // The protocol already finished for the orchestrator — nothing to do.
                (None, None, true)
            }
            None => {
                // A sender-driven Choice (the Critic gate): drive it client-side.
                (
                    None,
                    Some(json!({
                        "await": "critic_verdict",
                        "critic_iteration": row.critic_iteration,
                        "branches": ["pass", "revise"],
                    })),
                    false,
                )
            }
        };

    // 5. Re-claim the work-item lease for work_item_root (best-effort), then set
    //    status running.
    let lease_secs = params
        .lease_secs
        .unwrap_or(DEFAULT_LEASE_SECS)
        .clamp(10, 86_400);
    let orchestrator_agent = row
        .role_peer
        .get(&row.orchestrator_role)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let mut lease_reclaimed = false;
    let mut lease_expires_at = None;
    if let (Some(root), Some(agent)) =
        (row.work_item_root.as_deref(), orchestrator_agent.as_deref())
        && let Some(item) = get_work_item_by_public_id(pool, root)
            .await
            .map_err(|e| McpError::internal_error(format!("work-item lookup failed: {e}"), None))?
    {
        let claimed = crate::db::queries::claim_work_item(pool, item.id, agent, lease_secs)
            .await
            .map_err(|e| McpError::internal_error(format!("lease re-claim failed: {e}"), None))?;
        if let Some(r) = claimed {
            lease_reclaimed = true;
            lease_expires_at = r.lease_expires_at;
        }
    }

    // Transition status: done sessions go to `done`, otherwise back to `running`.
    let new_status = if done {
        SessionStatus::Done
    } else {
        SessionStatus::Running
    };
    mark_status(pool, &resumed_key, new_status)
        .await
        .map_err(|e| McpError::internal_error(format!("status transition failed: {e}"), None))?;

    json_result(&json!({
        "session_key": resumed_key,
        "forked_from": if params.fork { Some(params.session_key) } else { None },
        "status": new_status.as_str(),
        "conformant_prefix": true,
        "replayed_events": trace.len(),
        "next_step": next_step,
        "next_choice": next_choice,
        "done": done,
        "working_set": working_set_summary,
        "role_peer": row.role_peer,
        "cursor": row.cursor,
        "critic_iteration": row.critic_iteration,
        "critic_phase": row.critic_phase,
        "work_item_root": row.work_item_root,
        "experiment_ids": row.experiment_ids,
        "memory_scope": row.memory_scope,
        "lease_reclaimed": lease_reclaimed,
        "lease_expires_at": lease_expires_at,
    }))
}
