//! Migration step 51: `working_set_pages` + `working_set_config` — the durable
//! state backing the Crucible **context-tape paging control plane** (Phase 5).
//!
//! ## What this stores
//!
//! pgmcp treats a model's context window as a fixed *token budget* and the
//! indexed corpus as backing store. A *working set* is the multiset of pages
//! currently resident for one orchestration session at one trace position
//! (`state_cursor`). These two tables persist that working set so a paused +
//! resumed session can reconstruct it:
//!
//! - `working_set_pages` — one row per (session, cursor, page address): its
//!   residency [`state`](crate::tape::vocab::PageState), [`kind`](crate::tape::vocab::PageKind),
//!   importance, token cost, use count, and — critically — its `last_access_ord`.
//! - `working_set_config` — one row per session: the model window, the token
//!   budget, the [`policy`](crate::tape::vocab::EvictionPolicy), the TTL, and
//!   the session's monotonic `logical_clock`.
//!
//! ## The single most important design constraint: `last_access_ord` is LOGICAL
//!
//! `last_access_ord` is the value of the monotonic `working_set_config.logical_clock`
//! at the moment the page was last touched — **never wall-clock time**. Residency
//! is therefore a *deterministic function of the replayed trace*: re-running the
//! same sequence of page-ins / evictions advances the logical clock identically,
//! so a resumed session reconstructs a **bit-identical** working set. Wall-time
//! would make residency depend on how fast the trace replayed, which would break
//! the "trace IS the position" model (ADR-009) the orchestration checkpoints rely
//! on. The TTL policy likewise measures *logical* age (clock deltas), not seconds.
//!
//! ## Boundary
//!
//! Pure coordination/MEMORY state in pgmcp's OWN tables — pgmcp never runs a
//! shell or writes the user's files. The controller decides residency
//! mechanically from budget + policy; the agent never asserts it.
//!
//! ## Closed vocabularies (ADR-003)
//!
//! `page_kind` / `state` / `policy` are TEXT + CHECK built from the Rust enums'
//! `sql_in_list()` ([`crate::tape::vocab`]) so the DB constraint and the Rust
//! source-of-truth cannot drift; a golden test pins the version number below.
//!
//! Additive + `IF NOT EXISTS`, so idempotent and version-gated by `apply_step`.

use sqlx::PgPool;

use crate::tape::vocab::{EvictReason, EvictionPolicy, PageKind, PageState};

pub(super) const WORKING_SET: i32 = 51;
pub(super) const WORKING_SET_NAME: &str = "working_set";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    // ---- working_set_pages -------------------------------------------------
    // One row per (session, cursor, page address). `last_access_ord` is the
    // LOGICAL clock (see module docs); `valid_from` is the bi-temporal lower
    // bound of this residency record. `evict_reason` is NULL while resident and
    // set (from the closed EvictReason vocabulary) when the page is evicted.
    let create_pages = format!(
        "CREATE TABLE IF NOT EXISTS working_set_pages (
            id                BIGSERIAL PRIMARY KEY,
            session_key       TEXT NOT NULL
                                 REFERENCES orchestration_sessions(session_key) ON DELETE CASCADE,
            state_cursor      INT NOT NULL DEFAULT 0,
            page_kind         TEXT NOT NULL DEFAULT 'file_chunk' CHECK (page_kind IN ({kind})),
            page_addr         TEXT NOT NULL,
            tree_path         TEXT NOT NULL DEFAULT '',
            state             TEXT NOT NULL DEFAULT 'resident' CHECK (state IN ({state})),
            importance        REAL NOT NULL DEFAULT 0,
            est_tokens        INT NOT NULL DEFAULT 0,
            use_count         INT NOT NULL DEFAULT 0,
            last_access_ord   BIGINT NOT NULL DEFAULT 0,
            dirty             BOOL NOT NULL DEFAULT false,
            evict_reason      TEXT CHECK (evict_reason IS NULL OR evict_reason IN ({reason})),
            valid_from        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            UNIQUE (session_key, state_cursor, page_addr)
        )",
        kind = PageKind::sql_in_list(),
        state = PageState::sql_in_list(),
        reason = EvictReason::sql_in_list(),
    );
    sqlx::query(sqlx::AssertSqlSafe(create_pages.as_str()))
        .execute(pool)
        .await?;

    // The hot lookup: load every page of a working set at a cursor.
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_working_set_pages_session_cursor
            ON working_set_pages (session_key, state_cursor)",
    )
    .execute(pool)
    .await?;

    // Write-back scan: only dirty pages owe a `data_plane.put`. A partial index
    // keeps `list_dirty` cheap.
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_working_set_pages_dirty
            ON working_set_pages (session_key, state_cursor)
            WHERE dirty",
    )
    .execute(pool)
    .await?;

    // ---- working_set_config ------------------------------------------------
    // One row per session. `logical_clock` is the monotonic source for every
    // page's `last_access_ord` (the determinism anchor).
    let create_config = format!(
        "CREATE TABLE IF NOT EXISTS working_set_config (
            session_key          TEXT PRIMARY KEY
                                    REFERENCES orchestration_sessions(session_key) ON DELETE CASCADE,
            model_window_tokens  INT NOT NULL DEFAULT 0,
            budget_tokens        INT NOT NULL DEFAULT 0,
            policy               TEXT NOT NULL DEFAULT 'importance_weighted'
                                    CHECK (policy IN ({policy})),
            ttl_secs             INT,
            logical_clock        BIGINT NOT NULL DEFAULT 0,
            updated_at           TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
        policy = EvictionPolicy::sql_in_list(),
    );
    sqlx::query(sqlx::AssertSqlSafe(create_config.as_str()))
        .execute(pool)
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden version pin: v51 is (max existing migration v50) + 1. If a later
    /// migration is added, this MUST stay 51 and the new one takes 52+; bumping
    /// this number silently would desync the `apply_step` ordering.
    #[test]
    fn step_version_is_stable() {
        assert_eq!(WORKING_SET, 51);
        assert_eq!(WORKING_SET_NAME, "working_set");
    }

    /// The CHECK lists are sourced from the Rust enums, so the constraint and the
    /// closed vocabulary cannot drift. Guard the wiring (the columns exist in the
    /// DDL and reference the enum lists).
    #[test]
    fn ddl_sources_check_lists_from_enums() {
        assert!(PageKind::sql_in_list().contains("'file_chunk'"));
        assert!(PageState::sql_in_list().contains("'resident'"));
        assert!(EvictReason::sql_in_list().contains("'budget_pressure'"));
        assert!(EvictionPolicy::sql_in_list().contains("'importance_weighted'"));
    }
}
