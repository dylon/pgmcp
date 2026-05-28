//! Migration step 11: `nudge_emissions_v1`.
//!
//! Records every JIT adoption nudge (prompt-classifier) and tool-result hint so
//! (a) the observe pipeline can rate-limit per `(session_id, family)` and
//! (b) Phase 3 can measure nudge→adoption conversion. Local-only, same privacy
//! posture as `mcp_tool_calls` / `session_prompts` (no raw prompt text). See the
//! adoption plan `~/.claude/plans/how-can-the-agents-replicated-lighthouse.md`.

use sqlx::PgPool;

pub(super) const NUDGE_EMISSIONS_V1: i32 = 11;
pub(super) const NUDGE_EMISSIONS_V1_NAME: &str = "nudge_emissions_v1";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    // session_id is TEXT (the hook's session id, matching mcp_tool_calls.
    // mcp_session_id) rather than UUID, so it stores any client's session id
    // without a parse step.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS nudge_emissions (
            id          BIGSERIAL PRIMARY KEY,
            ts          TIMESTAMPTZ NOT NULL DEFAULT now(),
            session_id  TEXT NOT NULL,
            prompt_id   BIGINT,
            family      TEXT NOT NULL,
            channel     TEXT NOT NULL,
            client_name TEXT,
            project_id  INTEGER REFERENCES projects(id) ON DELETE SET NULL,
            CHECK (channel IN ('prompt', 'tool_result'))
        )",
    )
    .execute(pool)
    .await?;
    // Rate-limit lookup: recently nudged this (session, family)?
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_nudge_emissions_session_family_ts \
         ON nudge_emissions(session_id, family, ts)",
    )
    .execute(pool)
    .await?;
    // Conversion correlation (P3) scans by (client_name, ts).
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_nudge_emissions_client_ts \
         ON nudge_emissions(client_name, ts)",
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
        assert_eq!(NUDGE_EMISSIONS_V1, 11);
        assert_eq!(NUDGE_EMISSIONS_V1_NAME, "nudge_emissions_v1");
    }
}
