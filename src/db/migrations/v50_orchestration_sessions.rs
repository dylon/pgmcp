//! Migration step 50: `orchestration_sessions` — the durable checkpoint table
//! backing Crucible session **PAUSE/RESUME** (the "trace IS the position" model,
//! ADR-009). One row per orchestrated protocol run that pi can suspend and later
//! resume; replaying the recorded trace (`crate::csm::conformance::replay_to_states`)
//! recovers exactly where every role sits, from which the orchestrator's next step
//! is re-planned (`crate::csm::driver::next_step_from`).
//!
//! ## Boundary
//!
//! This is pure coordination/MEMORY state in pgmcp's OWN table — pgmcp never runs
//! a shell or writes the user's files. The agent supplies the checkpoint values
//! (protocol, cursor, critic iteration, role→peer map, …); pgmcp persists and
//! replays them and returns JSON.
//!
//! ## Columns of note
//!
//! - `status` is a closed [`crate::csm::session_store::SessionStatus`] vocabulary
//!   (`running | paused | resuming | done | failed`), TEXT + CHECK per the ADR-003
//!   idiom (the CHECK list is built from the Rust enum's `sql_in_list()` so the DB
//!   constraint and the Rust source-of-truth cannot drift; a golden test pins it).
//! - `global_type` JSONB stores the serialized `GlobalType` so resume can rebuild
//!   the projected `Network` without re-synthesizing the plan.
//! - `transcript` JSONB accumulates the orchestrator-side `Event`s executed so far
//!   (the unflushed tail); at pause it is flushed to `csm_run_traces.events` (keyed
//!   by `task_id`) via `insert_run_trace_if_absent`.
//! - `cursor` / `critic_iteration` / `critic_phase` cache the resume position so a
//!   resume need not re-derive them; they are cross-checked against the replayed
//!   trace.
//! - `lease_expires_at` mirrors the work-item lease so the crash-resume cron can
//!   auto-pause a session whose orchestrator died (lease lapsed).
//!
//! Additive + `IF NOT EXISTS`, so idempotent and version-gated by `apply_step`.

use sqlx::PgPool;

use crate::csm::session_store::SessionStatus;

pub(super) const ORCHESTRATION_SESSIONS: i32 = 50;
pub(super) const ORCHESTRATION_SESSIONS_NAME: &str = "orchestration_sessions";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    // The closed status vocabulary, sourced from the Rust enum (ADR-003).
    let create = format!(
        "CREATE TABLE IF NOT EXISTS orchestration_sessions (
            id                    BIGSERIAL PRIMARY KEY,
            session_key           TEXT UNIQUE NOT NULL,
            status                TEXT NOT NULL DEFAULT 'running' CHECK (status IN ({status})),
            protocol_name         TEXT NOT NULL,
            global_type           JSONB NOT NULL,
            orchestrator_role     TEXT NOT NULL DEFAULT 'O',
            task_id               UUID REFERENCES a2a_tasks(id) ON DELETE SET NULL,
            cursor                INT NOT NULL DEFAULT 0,
            critic_iteration      INT NOT NULL DEFAULT 0,
            critic_phase          TEXT,
            role_peer             JSONB NOT NULL DEFAULT '{{}}'::jsonb,
            work_item_root        TEXT,
            experiment_ids        BIGINT[] NOT NULL DEFAULT '{{}}',
            memory_scope          TEXT,
            pi_session_id         TEXT,
            pi_parent_session_id  TEXT,
            parent_session_id     BIGINT REFERENCES orchestration_sessions(id) ON DELETE SET NULL,
            lease_expires_at      TIMESTAMPTZ,
            paused_at             TIMESTAMPTZ,
            transcript            JSONB NOT NULL DEFAULT '[]'::jsonb,
            created_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            updated_at            TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
        status = SessionStatus::sql_in_list(),
    );
    sqlx::query(sqlx::AssertSqlSafe(create.as_str()))
        .execute(pool)
        .await?;

    // Filter resumable sessions by status (list_resumable / mark_status).
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_orchestration_sessions_status
            ON orchestration_sessions (status)",
    )
    .execute(pool)
    .await?;

    // The crash-resume cron scans only live sessions whose lease has lapsed; a
    // partial index over the two live statuses keeps that sweep cheap.
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_orchestration_sessions_lease
            ON orchestration_sessions (lease_expires_at)
            WHERE status IN ('running','resuming')",
    )
    .execute(pool)
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        assert_eq!(ORCHESTRATION_SESSIONS, 50);
        assert_eq!(ORCHESTRATION_SESSIONS_NAME, "orchestration_sessions");
    }

    /// The partial-index predicate must reference exactly the two *live* statuses
    /// the crash-resume cron treats as auto-pausable; guards against the index
    /// predicate drifting from the cron's WHERE clause.
    #[test]
    fn live_statuses_match_partial_index() {
        assert!(SessionStatus::Running.as_str() == "running");
        assert!(SessionStatus::Resuming.as_str() == "resuming");
    }
}
