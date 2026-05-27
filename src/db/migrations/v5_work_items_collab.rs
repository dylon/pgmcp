//! Migration step 5: `work_items_collab_v1` — the A2A collaboration layer for
//! the work-item tracker.
//!
//! Adds, on top of the v4 tracker schema:
//! - claim/lease state on `work_items` (`claimed_by`, `claimed_at`,
//!   `lease_expires_at`, `claim_count`) — co-located with `status` so a single
//!   atomic UPDATE claims-and-transitions;
//! - `work_item_claims` — an append-only claim/release/handoff/expire ledger
//!   (also the activity-feed source);
//! - `agent_presence` — activity-driven liveness keyed by the canonical
//!   free-text `agent_id` (most active agents never register in `a2a_agents`,
//!   so this is a table, not a column on the registry);
//! - `agent_identity` — a non-load-bearing view that enriches a free-text
//!   `agent_id` with `a2a_agents` registry metadata when `lower(name)` matches.
//!
//! Runs after v4 (needs `work_items`) and after the initial schema (needs
//! `a2a_agents` for the view). All statements idempotent. The canonical agent
//! identity is the free-text `agent_id` (decision 5); `a2a_tasks.requester_agent_id`
//! is intentionally left as-is (non-load-bearing — the view is the reconciliation).

use sqlx::PgPool;

pub(super) const WORK_ITEMS_COLLAB_V1: i32 = 5;
pub(super) const WORK_ITEMS_COLLAB_V1_NAME: &str = "work_items_collab_v1";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    // Claim/lease state on work_items.
    for stmt in [
        "ALTER TABLE work_items ADD COLUMN IF NOT EXISTS claimed_by TEXT",
        "ALTER TABLE work_items ADD COLUMN IF NOT EXISTS claimed_at TIMESTAMPTZ",
        "ALTER TABLE work_items ADD COLUMN IF NOT EXISTS lease_expires_at TIMESTAMPTZ",
        "ALTER TABLE work_items ADD COLUMN IF NOT EXISTS claim_count INTEGER NOT NULL DEFAULT 0",
        "CREATE INDEX IF NOT EXISTS idx_work_items_claimed_by ON work_items(claimed_by) \
            WHERE claimed_by IS NOT NULL",
    ] {
        sqlx::query(stmt).execute(pool).await?;
    }

    // Append-only claim ledger.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS work_item_claims (
            id            BIGSERIAL PRIMARY KEY,
            work_item_id  BIGINT NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
            agent_id      TEXT NOT NULL,
            action        TEXT NOT NULL CHECK (action IN
                              ('claim','release','handoff_out','handoff_in','expire','steal')),
            to_agent_id   TEXT,
            lease_expires_at TIMESTAMPTZ,
            created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await?;
    for idx in [
        "CREATE INDEX IF NOT EXISTS idx_wi_claims_item ON work_item_claims(work_item_id, created_at DESC)",
        "CREATE INDEX IF NOT EXISTS idx_wi_claims_agent ON work_item_claims(agent_id, created_at DESC)",
    ] {
        sqlx::query(idx).execute(pool).await?;
    }

    // Activity-driven presence.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS agent_presence (
            agent_id             TEXT PRIMARY KEY,
            last_active_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            status               TEXT NOT NULL DEFAULT 'active'
                                     CHECK (status IN ('active','idle','offline')),
            current_work_item_id BIGINT REFERENCES work_items(id) ON DELETE SET NULL,
            session_id           UUID,
            updated_at           TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_agent_presence_active ON agent_presence(last_active_at DESC)",
    )
    .execute(pool)
    .await?;

    // agent_identity bridge view: free-text agent_id ⇄ a2a_agents registry.
    // `DISTINCT ON (lower(name))` collapses case-variant duplicate registrations
    // to the most-recently-seen row.
    sqlx::query(
        "CREATE OR REPLACE VIEW agent_identity AS
            SELECT DISTINCT ON (lower(a.name))
                lower(a.name) AS agent_id,
                a.name        AS registered_name,
                a.url,
                a.specialty,
                a.recommended_role,
                a.last_seen_at
            FROM a2a_agents a
            ORDER BY lower(a.name), a.last_seen_at DESC NULLS LAST",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "INSERT INTO pgmcp_metadata (key, value) VALUES ('work_items_collab_version', '1')
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
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
        assert_eq!(WORK_ITEMS_COLLAB_V1, 5);
        assert_eq!(WORK_ITEMS_COLLAB_V1_NAME, "work_items_collab_v1");
    }
}
