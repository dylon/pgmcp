//! `DedupAction` — cross-path content-hash dedup decision branch.
//! Extracted from `pool.rs` as part of the D.2 god-file split.

/// Decision branch for cross-path content-hash dedup. Computed in the
/// embed worker after content extraction & hashing.
pub(super) enum DedupAction {
    /// Same content already indexed at this path — Level-2 skip.
    Level2Skip,
    /// Content already indexed at a different path that is now gone
    /// from disk. Update the canonical's path in place; reuse chunks
    /// and embeddings.
    Rename { canonical_id: i64, old_path: String },
    /// Content already indexed at a different path that is still
    /// present. Insert a metadata-only duplicate row pointing at the
    /// canonical; chunk queries dereference via `COALESCE`.
    Duplicate {
        canonical_id: i64,
        canonical_path: String,
    },
    /// No matching content elsewhere — proceed with the normal
    /// extract/chunk/embed/upsert path.
    ProceedNormal,
}
