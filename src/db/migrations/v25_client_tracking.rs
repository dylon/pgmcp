//! Migration step 25: `client_tracking_v1`.
//!
//! Adds `mcp_clients` — one row per connected MCP client, keyed by the
//! streamable-HTTP `mcp-session-id`. Records the client's OS **PID** + **working
//! directory** (recovered from `/proc` via the TCP peer address; see
//! `crate::proc_clients`), the resolved **project** (longest-prefix cwd match),
//! and **liveness**. Populated lazily on the first tool call of a session
//! (`StatsTracker::note_client`) and swept by the `mcp-client-liveness` cron
//! (`src/cron/mcp_client_liveness.rs`). This is the substrate for the
//! `active_clients` tool and the Phase-3 A2A active-agents-by-project view.
//!
//! `proc_start_ticks` (field 22 of `/proc/<pid>/stat`) fingerprints the process
//! incarnation, so a recycled PID is detected as a mismatch rather than a false
//! "still alive". Additive + `IF NOT EXISTS`, so it is idempotent and
//! version-gated (runs once).

use sqlx::PgPool;

pub(super) const CLIENT_TRACKING_V1: i32 = 25;
pub(super) const CLIENT_TRACKING_V1_NAME: &str = "client_tracking_v1";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS mcp_clients (
            mcp_session_id   TEXT PRIMARY KEY,
            client_name      TEXT NOT NULL,
            client_version   TEXT,
            protocol_version TEXT,
            pid              INTEGER,
            proc_start_ticks BIGINT,
            cwd              TEXT,
            project_id       INTEGER REFERENCES projects(id) ON DELETE SET NULL,
            first_seen       TIMESTAMPTZ NOT NULL DEFAULT now(),
            last_seen        TIMESTAMPTZ NOT NULL DEFAULT now(),
            last_liveness_at TIMESTAMPTZ,
            alive            BOOLEAN NOT NULL DEFAULT TRUE,
            exited_at        TIMESTAMPTZ
        )",
    )
    .execute(pool)
    .await?;

    // Partial indexes: the hot queries (`active_clients`, the A2A active-agents
    // view, the liveness sweep) all filter `WHERE alive`.
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_mcp_clients_project
            ON mcp_clients(project_id) WHERE alive",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_mcp_clients_pid
            ON mcp_clients(pid) WHERE alive",
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
        assert_eq!(CLIENT_TRACKING_V1, 25);
        assert_eq!(CLIENT_TRACKING_V1_NAME, "client_tracking_v1");
    }
}
