//! DB read/write for `orchestration_sessions` (ADR-009 PAUSE/RESUME).
//!
//! Mirrors `src/csm/store.rs`: pure persistence over pgmcp's OWN checkpoint
//! table — no protocol/replay logic lives here (that is in the sibling
//! `conformance` / `driver` modules). The functions here UPSERT a checkpoint,
//! load it back, list resumable sessions, transition status, fork a checkpoint
//! into a child run, and auto-pause crashed sessions whose lease lapsed.
//!
//! ## The "trace IS the position" model
//!
//! A checkpoint stores the serialized `GlobalType` (so resume can rebuild the
//! projected `Network`), the orchestrator-side `Event` transcript executed so far
//! (the unflushed tail), and cached resume hints (`cursor`, `critic_iteration`).
//! At pause the transcript is flushed to `csm_run_traces.events` via
//! [`crate::csm::store::insert_run_trace_if_absent`]; at resume the flushed events
//! plus any unflushed transcript replay through
//! [`crate::csm::conformance::replay_to_states`] to recover where every role sits.
//!
//! ## Boundary
//!
//! Every function is an ANALYTICAL/coordination/MEMORY operation: agent-provided
//! values + DB reads/writes to pgmcp's own table. pgmcp never runs a shell or
//! writes the user's files.

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::csm::conformance::Event;
use crate::tracker::kind::join_quoted;

/// The closed status vocabulary for `orchestration_sessions.status` (ADR-003
/// idiom: TEXT + CHECK built from this enum's [`SessionStatus::sql_in_list`], a
/// `#[cfg(test)]` golden test pins it — the same discipline as
/// [`crate::embed::failure_kind::FailureKind`]).
///
/// Lifecycle: a session is born `running`; an explicit pause (or the crash-resume
/// cron) moves it to `paused`; a resume sets `resuming` while it re-plans then
/// `running` again; it ends `done` (the protocol terminated) or `failed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionStatus {
    /// Actively orchestrated (the orchestrator holds a live work-item lease).
    Running,
    /// Suspended — its trace is flushed and the work-item lease dropped.
    Paused,
    /// Transient: a resume is re-planning the next step before going `running`.
    Resuming,
    /// The protocol reached a terminal state for every role.
    Done,
    /// The session aborted (the orchestrator gave up / an unrecoverable error).
    Failed,
}

impl SessionStatus {
    /// Canonical ordering; also the source of the DB CHECK vocabulary.
    pub const ALL: &'static [SessionStatus] = &[
        Self::Running,
        Self::Paused,
        Self::Resuming,
        Self::Done,
        Self::Failed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Resuming => "resuming",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.as_str() == s)
    }

    /// SQL `IN (...)` value list built from [`SessionStatus::ALL`] — the single
    /// source of truth shared with the v50 table's CHECK constraint.
    pub fn sql_in_list() -> String {
        join_quoted(Self::ALL.iter().map(|k| k.as_str()))
    }
}

/// One `orchestration_sessions` row, loaded back for resume / listing.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SessionCheckpoint {
    pub id: i64,
    pub session_key: String,
    pub status: String,
    pub protocol_name: String,
    pub global_type: Value,
    pub orchestrator_role: String,
    pub task_id: Option<Uuid>,
    pub cursor: i32,
    pub critic_iteration: i32,
    pub critic_phase: Option<String>,
    pub role_peer: Value,
    pub work_item_root: Option<String>,
    pub experiment_ids: Vec<i64>,
    pub memory_scope: Option<String>,
    pub pi_session_id: Option<String>,
    pub pi_parent_session_id: Option<String>,
    pub parent_session_id: Option<i64>,
    pub lease_expires_at: Option<DateTime<Utc>>,
    pub paused_at: Option<DateTime<Utc>>,
    pub transcript: Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl SessionCheckpoint {
    /// The orchestrator-side `Event`s recorded on this row's unflushed transcript
    /// (empty if none / malformed). The persisted shape is a JSON array of
    /// [`Event`]s; a malformed value is treated as no events (the flushed
    /// `csm_run_traces` copy is the durable record).
    pub fn transcript_events(&self) -> Vec<Event> {
        serde_json::from_value(self.transcript.clone()).unwrap_or_default()
    }
}

/// The agent-provided values folded into one UPSERT. All orchestration state is
/// supplied by the caller (the orchestrator), persisted verbatim here.
#[derive(Debug, Clone)]
pub struct CheckpointInput {
    pub session_key: String,
    pub status: SessionStatus,
    pub protocol_name: String,
    pub global_type: Value,
    pub orchestrator_role: String,
    pub task_id: Option<Uuid>,
    pub cursor: i32,
    pub critic_iteration: i32,
    pub critic_phase: Option<String>,
    pub role_peer: Value,
    pub work_item_root: Option<String>,
    pub experiment_ids: Vec<i64>,
    pub memory_scope: Option<String>,
    pub pi_session_id: Option<String>,
    pub pi_parent_session_id: Option<String>,
    pub parent_session_id: Option<i64>,
    pub lease_expires_at: Option<DateTime<Utc>>,
    pub paused_at: Option<DateTime<Utc>>,
    pub transcript: Value,
}

/// The columns selected back for [`SessionCheckpoint`] (kept in one place so the
/// SELECT list and the `FromRow` struct cannot drift).
const SESSION_COLS: &str = "id, session_key, status, protocol_name, global_type, \
    orchestrator_role, task_id, cursor, critic_iteration, critic_phase, role_peer, \
    work_item_root, experiment_ids, memory_scope, pi_session_id, pi_parent_session_id, \
    parent_session_id, lease_expires_at, paused_at, transcript, created_at, updated_at";

/// UPSERT a checkpoint by `session_key` — **idempotent**: re-saving the same key
/// overwrites the prior snapshot (a pause that is retried, or a periodic
/// checkpoint, lands on one row). Returns the row's id.
///
/// `created_at` is preserved on conflict; every other field is taken from
/// `input`, so the caller is the single source of truth for the snapshot.
pub async fn save_checkpoint(pool: &PgPool, input: &CheckpointInput) -> Result<i64, sqlx::Error> {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO orchestration_sessions
            (session_key, status, protocol_name, global_type, orchestrator_role, task_id,
             cursor, critic_iteration, critic_phase, role_peer, work_item_root, experiment_ids,
             memory_scope, pi_session_id, pi_parent_session_id, parent_session_id,
             lease_expires_at, paused_at, transcript, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16,
                 $17, $18, $19, NOW())
         ON CONFLICT (session_key) DO UPDATE SET
            status               = EXCLUDED.status,
            protocol_name        = EXCLUDED.protocol_name,
            global_type          = EXCLUDED.global_type,
            orchestrator_role    = EXCLUDED.orchestrator_role,
            task_id              = EXCLUDED.task_id,
            cursor               = EXCLUDED.cursor,
            critic_iteration     = EXCLUDED.critic_iteration,
            critic_phase         = EXCLUDED.critic_phase,
            role_peer            = EXCLUDED.role_peer,
            work_item_root       = EXCLUDED.work_item_root,
            experiment_ids       = EXCLUDED.experiment_ids,
            memory_scope         = EXCLUDED.memory_scope,
            pi_session_id        = EXCLUDED.pi_session_id,
            pi_parent_session_id = EXCLUDED.pi_parent_session_id,
            parent_session_id    = EXCLUDED.parent_session_id,
            lease_expires_at     = EXCLUDED.lease_expires_at,
            paused_at            = EXCLUDED.paused_at,
            transcript           = EXCLUDED.transcript,
            updated_at           = NOW()
         RETURNING id",
    )
    .bind(&input.session_key)
    .bind(input.status.as_str())
    .bind(&input.protocol_name)
    .bind(&input.global_type)
    .bind(&input.orchestrator_role)
    .bind(input.task_id)
    .bind(input.cursor)
    .bind(input.critic_iteration)
    .bind(input.critic_phase.as_deref())
    .bind(&input.role_peer)
    .bind(input.work_item_root.as_deref())
    .bind(&input.experiment_ids)
    .bind(input.memory_scope.as_deref())
    .bind(input.pi_session_id.as_deref())
    .bind(input.pi_parent_session_id.as_deref())
    .bind(input.parent_session_id)
    .bind(input.lease_expires_at)
    .bind(input.paused_at)
    .bind(&input.transcript)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Load a checkpoint by `session_key` (`Ok(None)` when no such session).
pub async fn load_checkpoint(
    pool: &PgPool,
    session_key: &str,
) -> Result<Option<SessionCheckpoint>, sqlx::Error> {
    let sql = format!("SELECT {SESSION_COLS} FROM orchestration_sessions WHERE session_key = $1");
    sqlx::query_as::<_, SessionCheckpoint>(sqlx::AssertSqlSafe(sql))
        .bind(session_key)
        .fetch_optional(pool)
        .await
}

/// List checkpoints that are resumable (`paused`), newest first, bounded. The
/// list tool surfaces these for an operator/orchestrator to pick up.
pub async fn list_resumable(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<SessionCheckpoint>, sqlx::Error> {
    let limit = limit.clamp(1, 10_000);
    let sql = format!(
        "SELECT {SESSION_COLS} FROM orchestration_sessions
          WHERE status = 'paused'
          ORDER BY COALESCE(paused_at, updated_at) DESC, id DESC
          LIMIT $1"
    );
    sqlx::query_as::<_, SessionCheckpoint>(sqlx::AssertSqlSafe(sql))
        .bind(limit)
        .fetch_all(pool)
        .await
}

/// Transition a session's `status` (e.g. `paused → resuming → running`). Returns
/// the updated row, or `Ok(None)` when no such session_key. `paused_at` is set
/// when moving to `paused` and cleared otherwise, so the partial crash-resume
/// index and the list ordering stay coherent.
pub async fn mark_status(
    pool: &PgPool,
    session_key: &str,
    status: SessionStatus,
) -> Result<Option<SessionCheckpoint>, sqlx::Error> {
    let sql = format!(
        "UPDATE orchestration_sessions
            SET status = $2,
                paused_at = CASE WHEN $2 = 'paused' THEN NOW() ELSE NULL END,
                updated_at = NOW()
          WHERE session_key = $1
          RETURNING {SESSION_COLS}"
    );
    sqlx::query_as::<_, SessionCheckpoint>(sqlx::AssertSqlSafe(sql))
        .bind(session_key)
        .bind(status.as_str())
        .fetch_optional(pool)
        .await
}

/// Fork an existing checkpoint into a fresh child session: copy every field of
/// `parent_session_key`, set `parent_session_id` to the parent's id, assign the
/// caller-supplied `new_session_key`, and mark it `running`. Returns the new row,
/// or `Ok(None)` when the parent does not exist.
///
/// The fork is a branch of the orchestration (e.g. exploring an alternative path
/// from the same paused position) — its own trace evolves independently.
pub async fn fork_checkpoint(
    pool: &PgPool,
    parent_session_key: &str,
    new_session_key: &str,
) -> Result<Option<SessionCheckpoint>, sqlx::Error> {
    let sql = format!(
        "INSERT INTO orchestration_sessions
            (session_key, status, protocol_name, global_type, orchestrator_role, task_id,
             cursor, critic_iteration, critic_phase, role_peer, work_item_root, experiment_ids,
             memory_scope, pi_session_id, pi_parent_session_id, parent_session_id,
             lease_expires_at, paused_at, transcript)
         SELECT $2, 'running', protocol_name, global_type, orchestrator_role, task_id,
                cursor, critic_iteration, critic_phase, role_peer, work_item_root, experiment_ids,
                memory_scope, pi_session_id, pi_parent_session_id, id,
                NULL, NULL, transcript
           FROM orchestration_sessions
          WHERE session_key = $1
         RETURNING {SESSION_COLS}"
    );
    sqlx::query_as::<_, SessionCheckpoint>(sqlx::AssertSqlSafe(sql))
        .bind(parent_session_key)
        .bind(new_session_key)
        .fetch_optional(pool)
        .await
}

/// Crash-resume sweep: auto-pause every live (`running`/`resuming`) session whose
/// work-item lease has lapsed (the orchestrator died mid-protocol). Bounded,
/// idempotent, and order-free. Returns the number of sessions paused.
///
/// This mirrors the work-item lease decay: a session whose orchestrator vanished
/// would otherwise sit `running` forever, never appearing in `list_resumable`.
pub async fn expire_stale_to_paused(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let res = sqlx::query(
        "UPDATE orchestration_sessions
            SET status = 'paused', paused_at = NOW(), updated_at = NOW()
          WHERE status IN ('running','resuming')
            AND lease_expires_at IS NOT NULL
            AND lease_expires_at < NOW()",
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn session_status_vocabulary_is_pinned() {
        let got: HashSet<&str> = SessionStatus::ALL.iter().map(|s| s.as_str()).collect();
        let expected: HashSet<&str> = ["running", "paused", "resuming", "done", "failed"]
            .into_iter()
            .collect();
        assert_eq!(got, expected, "SessionStatus vocabulary drifted");
    }

    #[test]
    fn session_status_round_trips() {
        for s in SessionStatus::ALL {
            assert_eq!(SessionStatus::parse(s.as_str()), Some(*s));
        }
        assert_eq!(SessionStatus::parse("bogus"), None);
    }

    #[test]
    fn sql_in_list_quotes_every_status() {
        let list = SessionStatus::sql_in_list();
        for s in SessionStatus::ALL {
            assert!(
                list.contains(&format!("'{}'", s.as_str())),
                "sql_in_list missing {}",
                s.as_str()
            );
        }
    }

    /// The resume INTERNALS (no DB): a GlobalType round-tripped through JSON
    /// (as it is stored on the checkpoint row) rebuilds a network, a 2-event
    /// prefix replays, and `next_step_from` yields the second step. This pins the
    /// pure logic the resume tool runs, independent of the DB harness.
    #[test]
    fn resume_internals_roundtrip_and_replay() {
        use crate::csm::conformance::{Event, replay_to_states};
        use crate::csm::driver::ProtocolDriver;
        use crate::csm::machine::Network;
        use crate::csm::mpst::global::{self, GlobalType};
        use crate::csm::role::{Label, Role};

        // A 2-worker linear protocol (what the synth tool folds for a 2-task plan).
        let o = Role::new("O");
        let g = global::interaction(
            o.clone(),
            Role::new("W0"),
            Label::text("t0_req"),
            global::interaction(
                Role::new("W0"),
                o.clone(),
                Label::text("t0_done"),
                global::interaction(
                    o.clone(),
                    Role::new("W1"),
                    Label::text("t1_req"),
                    global::interaction(
                        Role::new("W1"),
                        o.clone(),
                        Label::text("t1_done"),
                        global::end(),
                    ),
                ),
            ),
        );

        // Round-trip through JSON (the JSONB column shape).
        let json = serde_json::to_value(&g).expect("serialize");
        let g2: GlobalType = serde_json::from_value(json).expect("deserialize");
        assert_eq!(g, g2, "GlobalType must survive the JSON round-trip");

        let net = Network::build("synthesized:test", &g2).expect("network builds");
        let plan = ProtocolDriver::plan(&net, &o).expect("drivable");
        assert_eq!(plan.len(), 2);

        // Replay the first step's two events → recover the orchestrator position.
        let trace = vec![
            Event::new(o.clone(), "W0", Label::text("t0_req")),
            Event::new("W0", o.clone(), Label::text("t0_done")),
        ];
        let states = replay_to_states(&net, &trace).expect("prefix replays");
        let orch_state = *states.get(&o).expect("orchestrator tracked");
        let next = ProtocolDriver::next_step_from(&net, &o, orch_state).expect("a next step");
        assert_eq!(next, plan[1], "resume must surface plan()[1]");
    }
}
