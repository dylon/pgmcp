//! Migration step 30: `chunk_delete_index_hardening_v1`.
//!
//! `work_item_code_anchor.chunk_id` references `file_chunks(id) ON DELETE
//! CASCADE`, but v4 only indexed `item_id` and `file_id`. Chunk rows are
//! high-churn: the indexer replaces them on file updates, reindex operations,
//! and transcript refreshes. The missing child-side FK index makes those deletes
//! scan the full anchor table to enforce the cascade.

use sqlx::PgPool;

pub(super) const CHUNK_DELETE_INDEX_HARDENING_V1: i32 = 30;
pub(super) const CHUNK_DELETE_INDEX_HARDENING_V1_NAME: &str = "chunk_delete_index_hardening_v1";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_wi_anchor_chunk
            ON work_item_code_anchor(chunk_id)
            WHERE chunk_id IS NOT NULL",
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
        assert_eq!(CHUNK_DELETE_INDEX_HARDENING_V1, 30);
        assert_eq!(
            CHUNK_DELETE_INDEX_HARDENING_V1_NAME,
            "chunk_delete_index_hardening_v1"
        );
    }
}
