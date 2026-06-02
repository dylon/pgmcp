//! Migration step 24: `extracted_content_hash_v1`.
//!
//! Adds `indexed_files.extracted_content_hash BIGINT` — the `content_hash`
//! (xxHash3-64) recorded at a file's last successful symbol extraction. The
//! symbol-extraction cron (`src/cron/symbol_extraction.rs`) compares it to the
//! file's current `content_hash` and skips re-parsing when they match (RC2
//! incremental-skip). This keeps full re-scans affordable now that content-NULL
//! files are recovered from disk and parsed rather than filtered out — the
//! disk-fallback removed the `content IS NOT NULL` gate, so without this an
//! unchanged file would be re-read from disk and re-parsed on every full scan.
//!
//! Nullable + additive; `IF NOT EXISTS` keeps it idempotent and version-gated
//! (runs once). A NULL value means "never successfully extracted" → always
//! attempt extraction.

use sqlx::PgPool;

pub(super) const EXTRACTED_CONTENT_HASH_V1: i32 = 24;
pub(super) const EXTRACTED_CONTENT_HASH_V1_NAME: &str = "extracted_content_hash_v1";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "ALTER TABLE indexed_files
            ADD COLUMN IF NOT EXISTS extracted_content_hash BIGINT",
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
        assert_eq!(EXTRACTED_CONTENT_HASH_V1, 24);
        assert_eq!(EXTRACTED_CONTENT_HASH_V1_NAME, "extracted_content_hash_v1");
    }
}
