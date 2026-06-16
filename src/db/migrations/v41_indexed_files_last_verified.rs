//! Migration step 41: `indexed_files_last_verified`.
//!
//! Adds `indexed_files.last_verified_at TIMESTAMPTZ` — the wall-clock time the
//! indexer last *confirmed this row matches disk*, as distinct from `indexed_at`
//! / `modified_at` which freeze at the last **content** change. The scanner
//! advances `last_verified_at` on every Level-1 metadata skip, every Level-2
//! content-hash skip, and every full re-index (see `src/indexer/event_processor.rs`
//! and `src/embed/pool.rs`).
//!
//! Why: in a multi-branch workspace a `git checkout`/`rebase` bumps a file's
//! mtime without changing its content, so the Level-2 hash skip correctly avoids
//! re-embedding — but leaves `indexed_at` old. Any consumer comparing `indexed_at`
//! to disk mtime then reports a **false** "stale" (this fooled the 2026-05-21
//! index-staleness investigation). `last_verified_at` is the false-positive-free
//! freshness signal: it advances on the very pass that skips the git-touched file.
//!
//! Nullable + additive; `IF NOT EXISTS` keeps it idempotent and version-gated
//! (runs once). NULL means "never verified since the column was added" → the
//! first scan/reconcile pass fills it.

use sqlx::PgPool;

pub(super) const INDEXED_FILES_LAST_VERIFIED: i32 = 41;
pub(super) const INDEXED_FILES_LAST_VERIFIED_NAME: &str = "indexed_files_last_verified";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "ALTER TABLE indexed_files
            ADD COLUMN IF NOT EXISTS last_verified_at TIMESTAMPTZ",
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
        assert_eq!(INDEXED_FILES_LAST_VERIFIED, 41);
        assert_eq!(
            INDEXED_FILES_LAST_VERIFIED_NAME,
            "indexed_files_last_verified"
        );
    }
}
