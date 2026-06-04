//! Migration step 26: `client_file_events_v1`.
//!
//! Adds `client_file_events` — an append-only log of which client touched which
//! file, the substrate for the m:n client↔project attribution surfaced by
//! `client_project_matrix`. Rows arrive from three capture sources (`source`):
//! the Claude Code `PostToolUse` hook (`client_hook`), eBPF syscall tracing
//! (`ebpf`), and `/proc/<pid>/fd` sampling (`proc_fd`). `op` and `source` are
//! closed vocabularies (ADR-003): `TEXT` + a `CHECK` built from the `FileOp` /
//! `FileEventSource` enums via their `*_sql_in_list()` helpers, so the DB
//! constraint and the Rust types can never drift. Additive + `IF NOT EXISTS`, so
//! idempotent and version-gated (runs once).

use sqlx::PgPool;

use crate::proc_clients::file_events::{op_sql_in_list, source_sql_in_list};

pub(super) const CLIENT_FILE_EVENTS_V1: i32 = 26;
pub(super) const CLIENT_FILE_EVENTS_V1_NAME: &str = "client_file_events_v1";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    let create = format!(
        "CREATE TABLE IF NOT EXISTS client_file_events (
            id             BIGSERIAL PRIMARY KEY,
            mcp_session_id TEXT,
            session_id     UUID,
            pid            INTEGER,
            file_id        BIGINT  REFERENCES indexed_files(id) ON DELETE SET NULL,
            project_id     INTEGER REFERENCES projects(id) ON DELETE SET NULL,
            abs_path       TEXT NOT NULL,
            op             TEXT NOT NULL CHECK (op IN ({op})),
            source         TEXT NOT NULL CHECK (source IN ({source})),
            ts             TIMESTAMPTZ NOT NULL DEFAULT now()
        )",
        op = op_sql_in_list(),
        source = source_sql_in_list(),
    );
    sqlx::query(&create).execute(pool).await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_cfe_project_ts
            ON client_file_events(project_id, ts DESC)",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_cfe_pid_ts
            ON client_file_events(pid, ts DESC)",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_cfe_session
            ON client_file_events(mcp_session_id)",
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
        assert_eq!(CLIENT_FILE_EVENTS_V1, 26);
        assert_eq!(CLIENT_FILE_EVENTS_V1_NAME, "client_file_events_v1");
    }
}
