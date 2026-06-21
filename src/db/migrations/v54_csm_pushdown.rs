//! Migration step 54: pushdown / hierarchical CSM (ADR-030) — persistence for the
//! visibly-pushdown protocol **alphabet**, the sub-protocol **call graph**, and the
//! stack-aware PAUSE/RESUME **frame stack**.
//!
//! ## Why
//!
//! ADR-030 lifts the CSM from a finite-state CFSM to a *visibly pushdown* /
//! recursive-state machine: protocols gain `GlobalCall` / `GlobalBox` boundary
//! symbols (push/pop) and conformance keeps a per-role stack. The `GlobalType`
//! itself already round-trips through `csm_protocols.global_type` JSONB (adjacent
//! serde tagging, ADR-006), so **no schema change is needed to STORE pushdown
//! protocols**. This migration is therefore purely ANALYTICAL plus the one resume
//! column:
//!
//! - `csm_protocol_alphabet` — the visibly-pushdown alphabet: each protocol label
//!   paired with the [`StackAction`] it triggers (`neutral`/`push`/`pop`). The
//!   closed vocabulary follows the ADR-003 idiom — a `TEXT` column + a `CHECK`
//!   built from [`StackAction::sql_in_list`], pinned by the golden test in
//!   `src/csm/role.rs` — so the DB constraint and the Rust source-of-truth cannot
//!   drift.
//! - `csm_protocol_calls` — the sub-protocol call graph (caller → callee with the
//!   role-renaming `subst`), so the recursion / box structure is queryable (e.g.
//!   by the `csm_protocol_call_graph` tool).
//! - `orchestration_sessions.frame_stack` — the stack-aware resume position: a
//!   JSONB array of pending `(sub-protocol, cursor)` frames generalizing the flat
//!   scalar `cursor`. "The *stack of frames* IS the position" (ADR-030), the
//!   pushdown generalization of the v50 "the trace is the position" model; it is
//!   `[]` for a call-free (finite-state) session, so existing sessions are
//!   unaffected.
//!
//! Additive + idempotent (`CREATE TABLE IF NOT EXISTS`, `ADD COLUMN IF NOT
//! EXISTS`), so it is safe to re-run and version-gated by `apply_step`.

use sqlx::PgPool;

use crate::csm::role::StackAction;

pub(super) const CSM_PUSHDOWN: i32 = 54;
pub(super) const CSM_PUSHDOWN_NAME: &str = "csm_pushdown";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    // ---- the visibly-pushdown alphabet (label → stack action) ----
    // The closed `stack_action` vocabulary is sourced from the Rust enum (ADR-003),
    // so the CHECK and `StackAction::ALL` cannot drift (golden test in role.rs).
    let alphabet = format!(
        "CREATE TABLE IF NOT EXISTS csm_protocol_alphabet (
            id              BIGSERIAL PRIMARY KEY,
            protocol_name   TEXT NOT NULL,
            label           TEXT NOT NULL,
            stack_action    TEXT NOT NULL CHECK (stack_action IN ({sa})),
            ordinal         INT  NOT NULL DEFAULT 0,
            UNIQUE (protocol_name, label)
        )",
        sa = StackAction::sql_in_list(),
    );
    sqlx::query(sqlx::AssertSqlSafe(alphabet.as_str()))
        .execute(pool)
        .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_csm_protocol_alphabet_proto
            ON csm_protocol_alphabet (protocol_name)",
    )
    .execute(pool)
    .await?;

    // ---- the sub-protocol call graph (RSM boxes / recursion) ----
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS csm_protocol_calls (
            id          BIGSERIAL PRIMARY KEY,
            caller      TEXT  NOT NULL,
            callee      TEXT  NOT NULL,
            subst       JSONB NOT NULL DEFAULT '{}'::jsonb,
            ordinal     INT   NOT NULL DEFAULT 0
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_csm_protocol_calls_caller
            ON csm_protocol_calls (caller)",
    )
    .execute(pool)
    .await?;

    // ---- the stack-aware resume position (pushdown generalization of `cursor`) ----
    sqlx::query(
        "ALTER TABLE orchestration_sessions
            ADD COLUMN IF NOT EXISTS frame_stack JSONB NOT NULL DEFAULT '[]'::jsonb",
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
        assert_eq!(CSM_PUSHDOWN, 54);
        assert_eq!(CSM_PUSHDOWN_NAME, "csm_pushdown");
    }

    /// The alphabet CHECK vocabulary must be the StackAction closed set — the same
    /// source of truth the `src/csm/role.rs` golden test pins.
    #[test]
    fn alphabet_check_uses_stack_action_vocabulary() {
        assert_eq!(StackAction::sql_in_list(), "'neutral','push','pop'");
    }
}
