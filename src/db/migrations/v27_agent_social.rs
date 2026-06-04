//! Migration step 27: `agent_social_v1` — the A2A agent mailbox + presence
//! project-awareness (Phase 3).
//!
//! Adds:
//! - `agent_presence.current_project_id` — the project dimension that makes the
//!   presence/decay layer project-aware (paired with the `touch_presence`
//!   `session_id` population fix). Lets the active-agents-by-project view and the
//!   coordination layer ask "which agents are on project U".
//! - `agent_messages` — a mailbox to *live* agent instances (complementary to
//!   `a2a_send_task`'s spawn-RPC), addressable by session (precise instance),
//!   project (any agent there), or agent type. `kind` is a closed `MessageKind`
//!   vocabulary (ADR-003) covering the general envelopes *and* the Phase-4
//!   `WorktreeNegotiation` steps up front, so Phase 4 needs no schema change.
//! - `agent_message_receipts` — the m:n message↔recipient delivery ledger
//!   (dedup + per-channel rate-limit), `channel` a closed `DeliveryChannel`
//!   vocabulary.
//!
//! All additive + `IF NOT EXISTS`, so idempotent and version-gated.

use sqlx::PgPool;

use crate::a2a::mailbox::{channel_sql_in_list, kind_sql_in_list};

pub(super) const AGENT_SOCIAL_V1: i32 = 27;
pub(super) const AGENT_SOCIAL_V1_NAME: &str = "agent_social_v1";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    // Presence project-awareness.
    sqlx::query(
        "ALTER TABLE agent_presence
            ADD COLUMN IF NOT EXISTS current_project_id INTEGER
            REFERENCES projects(id) ON DELETE SET NULL",
    )
    .execute(pool)
    .await?;

    // Mailbox.
    let messages = format!(
        "CREATE TABLE IF NOT EXISTS agent_messages (
            id            BIGSERIAL PRIMARY KEY,
            from_agent    TEXT NOT NULL,
            from_session  TEXT,
            to_session    TEXT,
            to_project_id INTEGER REFERENCES projects(id) ON DELETE CASCADE,
            to_agent      TEXT,
            kind          TEXT NOT NULL CHECK (kind IN ({kind})),
            subject       TEXT,
            body          TEXT NOT NULL,
            reply_to      BIGINT REFERENCES agent_messages(id) ON DELETE SET NULL,
            created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
            expires_at    TIMESTAMPTZ,
            CHECK (to_session IS NOT NULL OR to_project_id IS NOT NULL OR to_agent IS NOT NULL)
        )",
        kind = kind_sql_in_list(),
    );
    sqlx::query(&messages).execute(pool).await?;

    // Delivery receipts (m:n message↔recipient; dedup via the UNIQUE key).
    let receipts = format!(
        "CREATE TABLE IF NOT EXISTS agent_message_receipts (
            id                BIGSERIAL PRIMARY KEY,
            message_id        BIGINT NOT NULL REFERENCES agent_messages(id) ON DELETE CASCADE,
            recipient_session TEXT,
            recipient_agent   TEXT,
            delivered_at      TIMESTAMPTZ,
            read_at           TIMESTAMPTZ,
            acked_at          TIMESTAMPTZ,
            channel           TEXT CHECK (channel IS NULL OR channel IN ({channel})),
            UNIQUE (message_id, recipient_session)
        )",
        channel = channel_sql_in_list(),
    );
    sqlx::query(&receipts).execute(pool).await?;

    // Addressing-dimension indexes (no time predicate — `now()` is not IMMUTABLE
    // and cannot appear in a partial-index WHERE; the inbox query filters live).
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_agent_msg_project ON agent_messages(to_project_id)",
    )
    .execute(pool)
    .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_agent_msg_session ON agent_messages(to_session)")
        .execute(pool)
        .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_agent_msg_agent ON agent_messages(to_agent)")
        .execute(pool)
        .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_agent_msg_receipt_session
            ON agent_message_receipts(recipient_session)",
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
        assert_eq!(AGENT_SOCIAL_V1, 27);
        assert_eq!(AGENT_SOCIAL_V1_NAME, "agent_social_v1");
    }
}
