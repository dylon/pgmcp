//! Migration step 18: `digest_emissions_v1` — Phase 4 proactive-digest ledger.
//!
//! Records every proactive digest emitted on a channel agents already read (the
//! SessionStart `pgmcp context` CLI and the UserPromptSubmit
//! `/api/session/observe` `additional_context`). Two jobs, exactly mirroring
//! `v11_nudge_emissions`:
//!
//! - the observe / context pipelines rate-limit per `(session_id, …)` and dedupe
//!   identical digests by `content_sha256` within a TTL (so the same standing
//!   state is not re-pushed every prompt), and
//! - it gives the digest a per-session emission cap (`max_per_session`).
//!
//! `channel` is CHECK-constrained from the closed
//! [`crate::digest::DigestChannel`] enum via its `sql_in_list()` (the same
//! `install_check` helper the v12/v17 migrations use). Local-only, same privacy
//! posture as `nudge_emissions` / `mcp_tool_calls` (no prompt text, no digest
//! body — only its sha256 fingerprint + an item count).
//!
//! TRUST NOTE: this table is the digest's ONLY write. The digest issues
//! `SELECT`s for everything it surfaces (tracker / health / trend) plus this one
//! INSERT into its own rate-limit ledger. `pgmcp-testing/tests/digest_trust_boundary.rs`
//! bans transition symbols from `src/digest/`.
//!
//! Version-gated (runs once); every statement is `IF NOT EXISTS` / idempotent.

use sqlx::PgPool;

pub(super) const DIGEST_EMISSIONS_V1: i32 = 18;
pub(super) const DIGEST_EMISSIONS_V1_NAME: &str = "digest_emissions_v1";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    // session_id is TEXT (the hook's session id, matching nudge_emissions /
    // mcp_tool_calls.mcp_session_id) rather than UUID, so the SessionStart CLI's
    // synthetic key and any client's session id both store without a parse step.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS digest_emissions (
            id             BIGSERIAL PRIMARY KEY,
            ts             TIMESTAMPTZ NOT NULL DEFAULT now(),
            session_id     TEXT NOT NULL,
            channel        TEXT NOT NULL,
            project_id     INTEGER REFERENCES projects(id) ON DELETE SET NULL,
            content_sha256 TEXT NOT NULL,
            item_count     INTEGER NOT NULL DEFAULT 0
        )",
    )
    .execute(pool)
    .await?;

    // channel CHECK, built from the closed DigestChannel enum (ADR-003 closed-
    // enum idiom; the same `install_check` helper the v12 bug-tracker and v17
    // git-links migrations use).
    super::v4_work_items::install_check(
        pool,
        "digest_emissions",
        "digest_emissions_channel_check",
        &format!("channel IN ({})", crate::digest::sql_in_list()),
    )
    .await?;

    // Per-session rate limit / cap lookup: how many digests, and when, for this
    // session (optionally scoped by channel).
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_digest_emissions_session_ts \
         ON digest_emissions(session_id, ts)",
    )
    .execute(pool)
    .await?;
    // Dedupe lookup: was this exact digest (by content fingerprint) emitted
    // within the TTL? Keyed by (content_sha256, ts).
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_digest_emissions_sha_ts \
         ON digest_emissions(content_sha256, ts)",
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
        // Pinning the constant — changing it is a schema-breaking event.
        assert_eq!(DIGEST_EMISSIONS_V1, 18);
        assert_eq!(DIGEST_EMISSIONS_V1_NAME, "digest_emissions_v1");
    }
}
