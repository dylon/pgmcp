//! Migration step 61: `subprocess_capture` — extend `client_file_events` for
//! whole-subtree (subprocess) file attribution via cgroup-v2 + eBPF (ADR-022,
//! E11), and widen the `source` CHECK for the new `ebpf_cgroup` capture source.
//!
//! ## What it adds
//!
//! - `client_file_events.cgroup_id BIGINT` — the cgroup-v2 id (the cgroup
//!   directory inode) of the acting process. Because cgroup membership is
//!   inherited across `fork`/`exec`, this is the stable key that attributes a
//!   `cargo`/`rustc` child back to the owning agent (joins to
//!   `mcp_clients.cgroup_id`). Stored as `BIGINT`; the kernel `u64` is bit-cast
//!   `as i64` (cgroup inodes fit below 2^63 in practice).
//! - `client_file_events.root_pid` / `ppid INTEGER` — advisory/forensic ancestry
//!   (the session-leader and immediate parent of the acting PID).
//! - `client_file_events.agent_id TEXT` — which agent produced the event
//!   (`claude-code` | `codex` | `pi` | …), orthogonal to `source` (the
//!   *mechanism*). Lets `client_project_matrix` attribute non-Claude hook rows
//!   correctly instead of the historical hardcoded `claude-code`.
//! - `mcp_clients.cgroup_id BIGINT` (+ a partial index `WHERE alive`) — the join
//!   target the eBPF cgroup probe resolves an event's owner against.
//!
//! ## Why the `source` CHECK is re-installed
//!
//! v26 created the table with an INLINE `CHECK (source IN (...))`, which Postgres
//! auto-names `client_file_events_source_check`. A `CREATE TABLE IF NOT EXISTS`
//! cannot widen that constraint on an already-migrated database, so adding the
//! `FileEventSource::EbpfCgroup` enum arm would be invisible to the live CHECK.
//! We DROP (IF EXISTS) and re-ADD it from the current `source_sql_in_list()` —
//! the v47 `install_check` idiom — so the DB constraint and the Rust closed
//! vocabulary (ADR-003) stay in lock-step. Additive + `IF NOT EXISTS`,
//! idempotent, version-gated (runs once).

use sqlx::PgPool;

use crate::proc_clients::file_events::source_sql_in_list;

pub(super) const SUBPROCESS_CAPTURE_V1: i32 = 61;
pub(super) const SUBPROCESS_CAPTURE_V1_NAME: &str = "subprocess_capture_v1";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    // ---- 1. client_file_events: subtree-attribution + agent columns --------
    for col in [
        "cgroup_id BIGINT",
        "root_pid  INTEGER",
        "ppid      INTEGER",
        "agent_id  TEXT",
    ] {
        let sql = format!("ALTER TABLE client_file_events ADD COLUMN IF NOT EXISTS {col}");
        sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
            .execute(pool)
            .await?;
    }

    // ---- 2. mcp_clients: the cgroup-id join target -------------------------
    sqlx::query("ALTER TABLE mcp_clients ADD COLUMN IF NOT EXISTS cgroup_id BIGINT")
        .execute(pool)
        .await?;
    // Partial index for the eBPF owner-resolution lookup (alive clients only),
    // mirroring the existing `WHERE alive` PID/project indexes on this table.
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_mcp_clients_cgroup
            ON mcp_clients(cgroup_id) WHERE alive",
    )
    .execute(pool)
    .await?;

    // ---- 3. attribution-by-cgroup recency index on the event log -----------
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_cfe_cgroup_ts
            ON client_file_events(cgroup_id, ts DESC)",
    )
    .execute(pool)
    .await?;

    // ---- 4. widen the `source` CHECK for `ebpf_cgroup` (v47 idiom) ---------
    super::v4_work_items::install_check(
        pool,
        "client_file_events",
        "client_file_events_source_check",
        &format!("source IN ({})", source_sql_in_list()),
    )
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        // Pinning the constant — changing it is a schema-breaking event.
        assert_eq!(SUBPROCESS_CAPTURE_V1, 61);
        assert_eq!(SUBPROCESS_CAPTURE_V1_NAME, "subprocess_capture_v1");
    }

    #[test]
    fn widened_source_check_includes_ebpf_cgroup() {
        // The CHECK is regenerated from the closed vocabulary, so the new arm
        // must be present in the list this migration installs.
        let list = source_sql_in_list();
        assert!(list.contains("'ebpf_cgroup'"), "got: {list}");
        assert!(list.contains("'client_hook'"), "got: {list}");
    }
}
