//! `session_checkpoint_save` — UPSERT a Crucible orchestration checkpoint and,
//! on `pause=true`, suspend the session (ADR-009 PAUSE).
//!
//! ## Boundary
//!
//! PERSIST-only. pgmcp writes the agent-provided checkpoint to its OWN
//! `orchestration_sessions` table, flushes the recorded trace to `csm_run_traces`,
//! and drops the work-item lease. It NEVER runs a shell or touches the user's
//! files; all orchestration state is supplied by the caller (pi).
//!
//! ## Pause guard (the trust boundary)
//!
//! A pause is refused unless every child `a2a_task` of `task_id` is terminal
//! (`TaskState::is_terminal`): pausing while a peer is still `Working` would strand
//! an in-flight turn whose result the replayed trace would never see. The refusal
//! returns `{paused:false, reason:"peer X is <state>"}` and logs at `warn!`
//! (a by-design trust-boundary "refused", per ADR-021), leaving the row `running`.
//!
//! On a granted pause: the transcript is flushed to `csm_run_traces.events` (keyed
//! by `task_id`, idempotent via `insert_run_trace_if_absent`), the cursor/critic
//! position is cached on the row (status `paused`), and the work-item lease on
//! `work_item_root` is released so another agent can pick the item up.

use std::sync::atomic::Ordering;

use chrono::Utc;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::{Value, json};
use sqlx::PgPool;
use tracing::{error, warn};
use uuid::Uuid;

use crate::a2a::types::TaskState;
use crate::context::SystemContext;
use crate::csm::conformance::Event;
use crate::csm::session_store::{CheckpointInput, SessionStatus, save_checkpoint};
use crate::csm::store::insert_run_trace_if_absent;
use crate::db::queries::get_work_item_by_public_id;
use crate::mcp::server::SessionCheckpointSaveParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err};

/// Default work-item lease window when the caller omits `lease_secs`.
const DEFAULT_LEASE_SECS: i64 = 900;

/// Parse the orchestrator's peer name from a `role → peer` JSON map (the value at
/// `orchestrator_role`), if present and a non-empty string. The lease is keyed by
/// this agent so resume can re-claim under the same identity.
fn orchestrator_agent(role_peer: &Value, orchestrator_role: &str) -> Option<String> {
    role_peer
        .get(orchestrator_role)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The first non-terminal child a2a_task of `task_id`, as `(id, state)` — the
/// pause guard's blocker. `Ok(None)` ⇒ all children terminal (or none exist).
async fn first_nonterminal_child(
    pool: &PgPool,
    task_id: Uuid,
) -> Result<Option<(Uuid, String)>, sqlx::Error> {
    let rows: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, status FROM a2a_tasks WHERE parent_task_id = $1")
            .bind(task_id)
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .find(|(_, status)| !TaskState::from_db_str(status).is_terminal()))
}

pub async fn tool_session_checkpoint_save(
    ctx: &SystemContext,
    params: SessionCheckpointSaveParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let pool = pool_or_err(ctx)?;

    let orchestrator_role = params
        .orchestrator_role
        .clone()
        .unwrap_or_else(|| "O".to_string());
    let role_peer = params
        .role_peer
        .clone()
        .unwrap_or_else(|| json!({ orchestrator_role.clone(): "pi" }));
    let transcript = params
        .transcript
        .clone()
        .unwrap_or_else(|| Value::Array(Vec::new()));

    // The transcript must be a well-formed CSM Event array — it is replayed on
    // resume, so a malformed value is a hard input error, not a silent drop.
    let events: Vec<Event> = serde_json::from_value(transcript.clone()).map_err(|e| {
        McpError::invalid_params(
            format!("transcript must be a JSON array of CSM Events {{from,to,label}}: {e}"),
            None,
        )
    })?;

    let task_id: Option<Uuid> = match params.task_id.as_deref() {
        Some(s) => Some(
            Uuid::parse_str(s)
                .map_err(|e| McpError::invalid_params(format!("bad task_id '{s}': {e}"), None))?,
        ),
        None => None,
    };

    let cursor = params.cursor.unwrap_or(0);
    let critic_iteration = params.critic_iteration.unwrap_or(0);
    let lease_secs = params
        .lease_secs
        .unwrap_or(DEFAULT_LEASE_SECS)
        .clamp(10, 86_400);
    let orchestrator = orchestrator_agent(&role_peer, &orchestrator_role);

    // ── PAUSE path ──────────────────────────────────────────────────────────
    if params.pause {
        // GUARD: refuse the pause while any child task is non-terminal.
        if let Some(tid) = task_id
            && let Some((child_id, child_state)) =
                first_nonterminal_child(pool, tid).await.map_err(|e| {
                    McpError::internal_error(format!("child-task scan failed: {e}"), None)
                })?
        {
            // Trust-boundary "refused": by-design, logs at warn! (ADR-021).
            warn!(
                %tid, %child_id, child_state,
                session_key = %params.session_key,
                "session_checkpoint_save: pause refused — a child task is still non-terminal"
            );
            return json_result(&json!({
                "session_key": params.session_key,
                "paused": false,
                "reason": format!("peer task {child_id} is {child_state}"),
            }));
        }

        // Flush the recorded trace to csm_run_traces (idempotent). Encoded series
        // is left empty here — the conformance/MSM cron owns the encoded form; this
        // is a checkpoint flush, not a validation. conformant=true marks it a
        // prefix we accepted as legal (the orchestrator only appends legal steps).
        let mut flushed_trace_id: Option<i64> = None;
        if let Some(tid) = task_id
            && !events.is_empty()
        {
            flushed_trace_id = insert_run_trace_if_absent(
                pool,
                tid,
                &params.protocol_name,
                true,
                None,
                &events,
                &[],
                None,
            )
            .await
            .map_err(|e| {
                McpError::internal_error(format!("trace flush to csm_run_traces failed: {e}"), None)
            })?;
        }

        // Flush the durable working set (the context-tape residency state) at the
        // suspend point, mirroring the transcript flush above: the paging engine
        // wrote pages incrementally to working_set_pages keyed by (session_key,
        // cursor); re-committing them here makes the resume-side load_working_set
        // reconstruct a bit-identical snapshot (the logical-clock determinism
        // guarantee). Keyed by the SAME cursor the checkpoint caches as its
        // position — "the trace IS the position". A missing working set is a
        // benign no-op (0 pages flushed). A DB fault here is ADR-021 error!-grade
        // (a real persistence failure), surfaced to the caller.
        let working_set_pages_flushed =
            crate::tape::store::flush_working_set(pool, &params.session_key, cursor)
                .await
                .map_err(|e| {
                    error!(
                        session_key = %params.session_key, cursor, error = %e,
                        "session_checkpoint_save: working-set flush failed during pause"
                    );
                    McpError::internal_error(format!("working-set flush failed: {e}"), None)
                })?;

        // Drop the work-item lease (best-effort: a missing/foreign-owned item is
        // not fatal — the lease may already have lapsed or never been claimed).
        let mut lease_dropped = false;
        if let (Some(root), Some(agent)) =
            (params.work_item_root.as_deref(), orchestrator.as_deref())
            && let Some(item) = get_work_item_by_public_id(pool, root).await.map_err(|e| {
                McpError::internal_error(format!("work-item lookup failed: {e}"), None)
            })?
        {
            let released = crate::db::queries::release_work_item(pool, item.id, agent)
                .await
                .map_err(|e| {
                    McpError::internal_error(format!("lease release failed: {e}"), None)
                })?;
            lease_dropped = released.is_some();
        }

        let input = CheckpointInput {
            session_key: params.session_key.clone(),
            status: SessionStatus::Paused,
            protocol_name: params.protocol_name.clone(),
            global_type: params.global_type.clone(),
            orchestrator_role,
            task_id,
            cursor,
            critic_iteration,
            critic_phase: params.critic_phase.clone(),
            role_peer,
            work_item_root: params.work_item_root.clone(),
            experiment_ids: params.experiment_ids.clone().unwrap_or_default(),
            memory_scope: params.memory_scope.clone(),
            pi_session_id: params.pi_session_id.clone(),
            pi_parent_session_id: params.pi_parent_session_id.clone(),
            parent_session_id: params.parent_session_id,
            // A paused session holds no lease.
            lease_expires_at: None,
            paused_at: Some(Utc::now()),
            transcript,
        };
        let id = save_checkpoint(pool, &input).await.map_err(|e| {
            McpError::internal_error(format!("checkpoint UPSERT failed: {e}"), None)
        })?;

        // Journal the graceful pause to the control-plane audit (ADR-020/D4) so the
        // operator can post-mortem when/why a session was checkpointed. Best-effort:
        // a journal failure must not fail the pause itself.
        let entry = crate::csm::trace_store::ControlInput {
            action: crate::csm::trace_store::ControlAction::Checkpoint,
            scope: crate::csm::trace_store::ControlScope::Session,
            session_key: Some(params.session_key.clone()),
            task_id: None,
            work_item_public_id: None,
            trace_id: None,
            span_id: None,
            reason: Some("pause".to_string()),
            actor: Some("mcp".to_string()),
            attributes: json!({ "cursor": cursor, "critic_iteration": critic_iteration }),
        };
        if let Err(e) = crate::csm::trace_store::record_control(pool, &entry).await {
            tracing::error!(error = %e, "checkpoint control-journal append failed (pause still applied)");
        }

        return json_result(&json!({
            "session_key": params.session_key,
            "id": id,
            "paused": true,
            "status": "paused",
            "cursor": cursor,
            "critic_iteration": critic_iteration,
            "trace_events_flushed": events.len(),
            "csm_run_trace_id": flushed_trace_id,
            "working_set_pages_flushed": working_set_pages_flushed,
            "lease_dropped": lease_dropped,
            "note": "session suspended; resume with session_checkpoint_resume(session_key)",
        }));
    }

    // ── RUNNING checkpoint (no pause) ───────────────────────────────────────
    // Refresh the work-item lease while running, so a periodic checkpoint keeps
    // the orchestrator's claim alive (and feeds the crash-resume cron's view).
    let mut lease_expires_at = None;
    let mut lease_refreshed = false;
    if let (Some(root), Some(agent)) = (params.work_item_root.as_deref(), orchestrator.as_deref())
        && let Some(item) = get_work_item_by_public_id(pool, root)
            .await
            .map_err(|e| McpError::internal_error(format!("work-item lookup failed: {e}"), None))?
    {
        let claimed = crate::db::queries::claim_work_item(pool, item.id, agent, lease_secs)
            .await
            .map_err(|e| McpError::internal_error(format!("lease (re)claim failed: {e}"), None))?;
        if let Some(r) = claimed {
            lease_expires_at = r.lease_expires_at;
            lease_refreshed = true;
        }
    }

    let input = CheckpointInput {
        session_key: params.session_key.clone(),
        status: SessionStatus::Running,
        protocol_name: params.protocol_name.clone(),
        global_type: params.global_type.clone(),
        orchestrator_role,
        task_id,
        cursor,
        critic_iteration,
        critic_phase: params.critic_phase.clone(),
        role_peer,
        work_item_root: params.work_item_root.clone(),
        experiment_ids: params.experiment_ids.clone().unwrap_or_default(),
        memory_scope: params.memory_scope.clone(),
        pi_session_id: params.pi_session_id.clone(),
        pi_parent_session_id: params.pi_parent_session_id.clone(),
        parent_session_id: params.parent_session_id,
        lease_expires_at,
        paused_at: None,
        transcript,
    };
    let id = save_checkpoint(pool, &input)
        .await
        .map_err(|e| McpError::internal_error(format!("checkpoint UPSERT failed: {e}"), None))?;

    json_result(&json!({
        "session_key": params.session_key,
        "id": id,
        "paused": false,
        "status": "running",
        "cursor": cursor,
        "critic_iteration": critic_iteration,
        "lease_refreshed": lease_refreshed,
        "lease_expires_at": lease_expires_at,
    }))
}
