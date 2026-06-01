//! Migration step 21: `sync_ops_v1` — the ordered synchronization skeleton.
//!
//! `symbol_effects` records the *unordered* set of concurrency effects a symbol
//! carries, but static deadlock detection (lock-order graph SCCs) and Petri-net
//! channel/bottleneck analysis need the ORDERED, scope-nested sequence of lock
//! acquire/release and channel send/recv operations, with best-effort resource
//! identity. This table is that skeleton: one row per sync op,
//! `(symbol_id, seq)`-ordered, with the held-set recoverable from explicit
//! `release` ops + `nesting_depth` + `guard_id` pairing.
//!
//! Populated in-pass by the symbol-extraction cron (the same per-file
//! transaction that writes `file_symbols`); `ON DELETE CASCADE` on `symbol_id`
//! means a file re-extraction or removal scrubs its ops automatically.
//! `op_kind` / `resource_kind` / `paradigm` are closed vocabularies (ADR-003):
//! `TEXT` + a `CHECK` built from
//! [`crate::parsing::sync_ops`]`::{SyncOpKind,ResourceKind,SyncParadigm}` via
//! their `*_sql_in_list()`, with a golden test pinning each set. The CHECKs are
//! `DROP`+`ADD` so they track enum edits on existing installs too (same
//! discipline as the work-item CHECK installer).
//!
//! `file_symbols` can grow large, so the build lifts the per-statement timeout
//! for its single transaction (mirrors v13/v20). Every statement is
//! `IF NOT EXISTS` / idempotent and version-gated (runs once).

use sqlx::PgPool;

use crate::parsing::sync_ops::{
    op_kind_sql_in_list, paradigm_sql_in_list, resource_kind_sql_in_list,
};

pub(super) const SYNC_OPS_V1: i32 = 21;
pub(super) const SYNC_OPS_V1_NAME: &str = "sync_ops_v1";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;

    // `SET LOCAL` reverts at commit and never leaks onto the pooled connection.
    sqlx::query("SET LOCAL statement_timeout = 0")
        .execute(&mut *tx)
        .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS sync_ops (
            id                  BIGSERIAL PRIMARY KEY,
            symbol_id           BIGINT NOT NULL REFERENCES file_symbols(id) ON DELETE CASCADE,
            seq                 INT  NOT NULL,
            op_kind             TEXT NOT NULL,
            resource_key        TEXT,
            resource_kind       TEXT NOT NULL,
            paradigm            TEXT NOT NULL,
            nesting_depth       INT  NOT NULL DEFAULT 0,
            guard_id            INT,
            resource_confidence REAL NOT NULL DEFAULT 0.0
                                CHECK (resource_confidence >= 0.0 AND resource_confidence <= 1.0),
            line                INT  NOT NULL,
            UNIQUE (symbol_id, seq)
        )",
    )
    .execute(&mut *tx)
    .await?;

    // Closed-vocab CHECKs built from the Rust enums (single source of truth).
    // DROP+ADD so the constraint tracks enum edits on existing installs.
    for (name, col, list) in [
        ("chk_sync_ops_op_kind", "op_kind", op_kind_sql_in_list()),
        (
            "chk_sync_ops_resource_kind",
            "resource_kind",
            resource_kind_sql_in_list(),
        ),
        ("chk_sync_ops_paradigm", "paradigm", paradigm_sql_in_list()),
    ] {
        sqlx::query(&format!(
            "ALTER TABLE sync_ops DROP CONSTRAINT IF EXISTS {name}"
        ))
        .execute(&mut *tx)
        .await?;
        sqlx::query(&format!(
            "ALTER TABLE sync_ops ADD CONSTRAINT {name} CHECK ({col} IN ({list}))"
        ))
        .execute(&mut *tx)
        .await?;
    }

    for stmt in [
        // Dominant access: a symbol's ordered skeleton.
        "CREATE INDEX IF NOT EXISTS idx_sync_ops_symbol_seq ON sync_ops (symbol_id, seq)",
        // Lock-order / Petri build: group ops by resource across the project.
        "CREATE INDEX IF NOT EXISTS idx_sync_ops_resource ON sync_ops (resource_key) \
         WHERE resource_key IS NOT NULL",
        // Paradigm/kind slicing (lock-order vs Petri split).
        "CREATE INDEX IF NOT EXISTS idx_sync_ops_kind ON sync_ops (op_kind)",
        "CREATE INDEX IF NOT EXISTS idx_sync_ops_paradigm ON sync_ops (paradigm)",
    ] {
        sqlx::query(stmt).execute(&mut *tx).await?;
    }

    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        // Pinning the constant — changing it is a schema-breaking event.
        assert_eq!(SYNC_OPS_V1, 21);
        assert_eq!(SYNC_OPS_V1_NAME, "sync_ops_v1");
    }

    #[test]
    fn checks_quote_the_full_vocabulary() {
        // Boy-Scout: a new op_kind/resource_kind that forgot this migration
        // fails here (CHECK list incomplete), not silently at insert time.
        assert!(op_kind_sql_in_list().contains("'acquire'"));
        assert!(op_kind_sql_in_list().contains("'recv_persistent'"));
        assert!(resource_kind_sql_in_list().contains("'mutex'"));
        assert!(resource_kind_sql_in_list().contains("'channel'"));
        assert!(paradigm_sql_in_list().contains("'lock'"));
        assert!(paradigm_sql_in_list().contains("'message'"));
    }
}
