//! Migration step 13: `fts_stored_tsv_v1`.
//!
//! Replaces the on-the-fly `to_tsvector('english', content)` expression — both
//! the expression GIN index (`idx_file_chunks_fts`) and the per-row recompute
//! inside `text_search` / `text_search_bounded` / `hybrid_search_chunks` — with
//! a STORED generated column `file_chunks.content_tsv` plus a dedicated
//! `fastupdate=off` GIN index. This makes the BM25/full-text leg fast and
//! predictable: the heavy tokenization is materialized at write time, and
//! turning off the GIN pending list removes the post-restart "pending-list
//! flush" stall that let the leg exceed `statement_timeout` under a write storm
//! (the operational trigger of the `hybrid_search` timeout bug).
//!
//! Adding a STORED generated column rewrites `file_chunks`, which can far exceed
//! the pooled connection's 30s `statement_timeout`; the whole step therefore
//! runs in one transaction with `SET LOCAL statement_timeout = 0`. The migration
//! runner's retry wrapper only retries lock collisions (55P03), NOT a
//! statement-timeout cancel (57014), so without this the rewrite would abort
//! startup on any non-trivial corpus. The step is version-gated (runs once) and
//! every statement is `IF [NOT] EXISTS` / idempotent, so a retried or partial
//! apply is safe.
//!
//! See the plan `~/.claude/plans/plan-fixes-for-the-vectorized-tulip.md`.

use sqlx::PgPool;

pub(super) const FTS_STORED_TSV_V1: i32 = 13;
pub(super) const FTS_STORED_TSV_V1_NAME: &str = "fts_stored_tsv_v1";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;

    // Lift the per-statement timeout for THIS transaction only. `SET LOCAL`
    // reverts at commit/rollback and never leaks onto the pooled connection.
    // The `ADD COLUMN` below rewrites the whole table, which can take minutes
    // on a large corpus — well past the daemon-wide 30s ceiling.
    sqlx::query("SET LOCAL statement_timeout = 0")
        .execute(&mut *tx)
        .await?;

    // Stored, materialized tsvector. `to_tsvector('english', content)` is the
    // 2-arg (explicit-regconfig) form, which is IMMUTABLE — required for a
    // GENERATED column, and identical to the expression the old index used, so
    // query semantics are unchanged. NULL `content` → NULL tsvector → no match,
    // exactly as before.
    sqlx::query(
        "ALTER TABLE file_chunks
            ADD COLUMN IF NOT EXISTS content_tsv tsvector
            GENERATED ALWAYS AS (to_tsvector('english', content)) STORED",
    )
    .execute(&mut *tx)
    .await?;

    // Dedicated GIN over the stored column. `fastupdate = off` writes straight
    // into the index instead of a pending list, so the first query after a
    // restart / bulk-index storm doesn't pay a large pending-list flush — the
    // operational trigger of the original timeout.
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_file_chunks_content_tsv
            ON file_chunks USING gin(content_tsv) WITH (fastupdate = off)",
    )
    .execute(&mut *tx)
    .await?;

    // Retire the legacy expression index — every full-text query now reads
    // `content_tsv`, and the initial-schema block no longer recreates it.
    sqlx::query("DROP INDEX IF EXISTS idx_file_chunks_fts")
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
        assert_eq!(FTS_STORED_TSV_V1, 13);
        assert_eq!(FTS_STORED_TSV_V1_NAME, "fts_stored_tsv_v1");
    }
}
