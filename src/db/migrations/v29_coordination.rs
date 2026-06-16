//! Migration step 29: `coordination_v1` (Phase 4) — the worktree-coordination
//! state machine that realizes the formally-verified `WorktreeNegotiation`
//! protocol (`docs/formal/WorktreeNegotiation.{tla,v}`).
//!
//! - `coordination_requests` — one negotiation between a dependent project's
//!   agent (requester) and a dependency project's agent (editor). `status` is a
//!   closed `CoordinationStatus` vocabulary; an agent can reach `moved` (a
//!   candidate) but **only** the git-scanner's `stable_restored` `project_event`
//!   may set `resolved` (the gatekeeper trust boundary).
//! - `project_events` — git-state events the scanner posts (the gatekeeper seam,
//!   paralleling `ci_evidence`/`pr_event`).
//!
//! `coordination_requests.blocked_work_item_id` is the §4.5 close-the-loop link:
//! when a dependent's agent names a blocked work-item, `coordinate_dependency_block`
//! sets it `blocked` (Actor::Agent) and records it here; the git-scanner gatekeeper
//! (`resolve_and_notify`) then flips it `blocked → ready` as `Actor::System` — the
//! editor (Agent) can never reach that transition, mirroring the v17 CI-evidence
//! gate.
//!
//! Additive + `IF NOT EXISTS`, so idempotent and version-gated.

use sqlx::PgPool;

use crate::deps::coordination::{event_kind_sql_in_list, status_sql_in_list};

pub(super) const COORDINATION_V1: i32 = 29;
pub(super) const COORDINATION_V1_NAME: &str = "coordination_v1";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    let coord = format!(
        "CREATE TABLE IF NOT EXISTS coordination_requests (
            id BIGSERIAL PRIMARY KEY,
            dependent_project_id  INTEGER REFERENCES projects(id) ON DELETE CASCADE,
            dependency_project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            requester_agent   TEXT,
            requester_session TEXT,
            editor_session    TEXT,
            status            TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ({status})),
            reason            TEXT,
            error_excerpt     TEXT,
            worktree_branch   TEXT,
            message_id        BIGINT REFERENCES agent_messages(id) ON DELETE SET NULL,
            blocked_work_item_id BIGINT REFERENCES work_items(id) ON DELETE SET NULL,
            created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
            resolved_at       TIMESTAMPTZ
        )",
        status = status_sql_in_list(),
    );
    sqlx::query(sqlx::AssertSqlSafe(coord.as_str()))
        .execute(pool)
        .await?;

    let events = format!(
        "CREATE TABLE IF NOT EXISTS project_events (
            id BIGSERIAL PRIMARY KEY,
            project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            kind       TEXT NOT NULL CHECK (kind IN ({kind})),
            payload    JSONB NOT NULL DEFAULT '{{}}'::jsonb,
            created_at TIMESTAMPTZ NOT NULL DEFAULT now()
        )",
        kind = event_kind_sql_in_list(),
    );
    sqlx::query(sqlx::AssertSqlSafe(events.as_str()))
        .execute(pool)
        .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_coord_dependency_open
            ON coordination_requests (dependency_project_id)
            WHERE status IN ('pending','accepted','moved')",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_project_events_project
            ON project_events (project_id, created_at DESC)",
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
        assert_eq!(COORDINATION_V1, 29);
        assert_eq!(COORDINATION_V1_NAME, "coordination_v1");
    }
}
