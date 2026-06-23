//! Migration step 62: widen the `client_file_events.source` CHECK for the new
//! `preload` capture source — the unprivileged `LD_PRELOAD` libc-interposition
//! shim (ADR-022 Phase 2D).
//!
//! v26 created the column with an INLINE `CHECK (source IN (...))`, which Postgres
//! auto-names `client_file_events_source_check`; a `CREATE TABLE IF NOT EXISTS`
//! cannot widen it on an already-migrated database, so the new
//! `FileEventSource::Preload` arm would be invisible to the live CHECK. We DROP
//! (IF EXISTS) and re-ADD it from the current `source_sql_in_list()` — the
//! v47/v61 `install_check` idiom — so the DB constraint and the Rust closed
//! vocabulary (ADR-003) stay in lock-step.
//!
//! No column changes: the `agent_id` / `cgroup_id` / `ppid` columns the preload
//! path populates were all added by v61. Idempotent, version-gated.

use sqlx::PgPool;

use crate::proc_clients::file_events::source_sql_in_list;

pub(super) const PRELOAD_SOURCE_V1: i32 = 62;
pub(super) const PRELOAD_SOURCE_V1_NAME: &str = "preload_source_v1";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
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
        assert_eq!(PRELOAD_SOURCE_V1, 62);
        assert_eq!(PRELOAD_SOURCE_V1_NAME, "preload_source_v1");
    }

    #[test]
    fn widened_source_check_includes_preload() {
        // The CHECK is regenerated from the closed vocabulary, so the new arm
        // must be present and the prior arms must not have been dropped.
        let list = source_sql_in_list();
        assert!(list.contains("'preload'"), "got: {list}");
        assert!(list.contains("'ebpf_cgroup'"), "got: {list}");
        assert!(list.contains("'client_hook'"), "got: {list}");
    }
}
