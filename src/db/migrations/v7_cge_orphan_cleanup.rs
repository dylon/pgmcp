//! Migration step 7: `cge_orphan_cleanup_v1` — one-time removal of the
//! orphan `code_graph_edges` rows left behind by the old
//! `target_file_id ON DELETE SET NULL` behavior (the FK is now CASCADE; see
//! the re-tighten DO block in `migrations.rs` and
//! `docs/scientific-ledger/idx-cge-unique-set-null-collision-2026-05-27.md`).
//!
//! Before the FK was re-tightened, deleting an `indexed_files` row that an
//! edge pointed at did not remove the edge — it nulled `target_file_id`,
//! leaving a `(source, NULL, edge_type, …)` orphan. For **semantic** and
//! **co-change** edges (`target_raw IS NULL`) a NULL target is meaningless:
//! those edge types are file→file relationships always inserted with a real
//! `target_file_id`, so any surviving `target_file_id IS NULL` row of that
//! shape is definitionally an orphan. **Import** orphans carry the import
//! string in `target_raw` (NOT NULL) and are legitimate *unresolved* imports,
//! so they are preserved by the `target_raw IS NULL` predicate.
//!
//! This is version-gated (runs exactly once) rather than inline-on-every-boot:
//! the DELETE is a full scan, and once the FK is CASCADE no new
//! `target_file_id IS NULL AND target_raw IS NULL` row is ever created, so a
//! second run would only re-scan to delete zero rows.

use sqlx::PgPool;
use tracing::info;

/// Step version number — must be unique across all migration steps.
pub(super) const CGE_ORPHAN_CLEANUP_V1: i32 = 7;
pub(super) const CGE_ORPHAN_CLEANUP_V1_NAME: &str = "cge_orphan_cleanup_v1";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    let deleted = sqlx::query(
        "DELETE FROM code_graph_edges
          WHERE target_file_id IS NULL AND target_raw IS NULL",
    )
    .execute(pool)
    .await?
    .rows_affected();

    info!(
        deleted_orphan_edges = deleted,
        "cge_orphan_cleanup_v1: removed semantic/co-change edges orphaned by the \
         old target_file_id SET NULL behavior (import orphans preserved)"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        assert_eq!(CGE_ORPHAN_CLEANUP_V1, 7);
        assert_eq!(CGE_ORPHAN_CLEANUP_V1_NAME, "cge_orphan_cleanup_v1");
    }
}
