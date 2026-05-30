//! Migration step 20: `unresolved_ref_index_v1`.
//!
//! A partial index over *unresolved* `symbol_references` rows
//! (`WHERE resolution_kind IS NULL`).
//!
//! `resolve_symbol_reference_targets` (and its cheap pre-pass backlog guard in
//! `src/db/queries/symbols.rs`) filter on `resolution_kind IS NULL`. Indexing
//! exactly those rows makes the per-cron "is there a backlog to drain?" EXISTS
//! probe near-O(1) on a fully-resolved project (the partial index is empty for
//! it) and accelerates each resolution phase's `IS NULL` scan. This is what
//! makes it cheap for the symbol-extraction cron to run resolution even on a
//! "no new files" cycle, which is required to drain references stranded by the
//! pre-fix Phase-3 timeout (a resolution that 300s-cancelled returned before the
//! watermark advanced; a later no-files run then advanced the watermark past the
//! still-NULL rows). See
//! `~/.claude/plans/pgmcp-has-not-logged-structured-sprout.md`.
//!
//! `symbol_references` can hold millions of rows, so the build lifts the
//! per-statement timeout for its single transaction (mirrors v13). The statement
//! is `IF NOT EXISTS` / idempotent and version-gated (runs once).

use sqlx::PgPool;

pub(super) const UNRESOLVED_REF_INDEX_V1: i32 = 20;
pub(super) const UNRESOLVED_REF_INDEX_V1_NAME: &str = "unresolved_ref_index_v1";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;

    // The partial index build scans `symbol_references`, which can far exceed the
    // pooled connection's 30s `statement_timeout` on a large corpus. `SET LOCAL`
    // reverts at commit and never leaks onto the pooled connection.
    sqlx::query("SET LOCAL statement_timeout = 0")
        .execute(&mut *tx)
        .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_symbol_refs_unresolved
            ON symbol_references(source_file_id)
            WHERE resolution_kind IS NULL",
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        // Pinning the constant — changing it is a schema-breaking event.
        assert_eq!(UNRESOLVED_REF_INDEX_V1, 20);
        assert_eq!(UNRESOLVED_REF_INDEX_V1_NAME, "unresolved_ref_index_v1");
    }
}
