//! Migration step 42: `index_failures` — the content-intrinsic failure ledger.
//!
//! A small append-and-update table keyed on `path`, recording files that fail
//! indexing for a *content-intrinsic* reason (non-UTF-8, document-extraction
//! failure/timeout/OOM — see [`crate::embed::failure_kind::FailureKind`]). Two
//! purposes:
//!   1. **Bounded retry.** Without it, the reconcile cron
//!      (`src/cron/index_reconcile.rs`) re-walks every workspace and re-submits
//!      these permanently-bad files on every tick — re-running `pandoc` on a
//!      corrupt 50 MiB PDF every 30 minutes forever. The scanner consults this
//!      table and stops re-submitting a file once `failure_count >=
//!      [indexer] max_index_retries` *and its mtime has not advanced* past the
//!      last failure (an edit lifts the bound).
//!   2. **Visibility.** `index_stats` surfaces a `failure_kind` breakdown so the
//!      opaque `files_failed` counter becomes actionable.
//!
//! Transient infrastructure failures (DB upsert/replace timeouts) are
//! deliberately NOT recorded here — they self-heal on the next reconcile and
//! recording them would mean writing to a possibly-down database.
//!
//! The `failure_kind` TEXT column carries a closed-vocab `CHECK` built from
//! `FailureKind` via the stamp-aware `install_check` (ADR-003 idiom, re-applied
//! on a later enum change via DROP+ADD). A successful (re)index clears the row
//! (`DELETE FROM index_failures WHERE path = $1` inside `replace_indexed_file` /
//! `insert_duplicate_file` / `update_file_path_in_place`). Additive, idempotent,
//! version-gated.

use sqlx::PgPool;

use crate::embed::failure_kind;

pub(super) const INDEX_FAILURES: i32 = 42;
pub(super) const INDEX_FAILURES_NAME: &str = "index_failures";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS index_failures (
            path            TEXT        PRIMARY KEY,
            failure_kind    TEXT        NOT NULL,
            failure_count   INTEGER     NOT NULL DEFAULT 1,
            first_failed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
            last_failed_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
            last_error      TEXT
        )",
    )
    .execute(pool)
    .await?;

    // Scanner bounded-failure lookup: bounded files past the retry cap.
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS ix_index_failures_count \
            ON index_failures (failure_count)",
    )
    .execute(pool)
    .await?;

    super::v4_work_items::install_check(
        pool,
        "index_failures",
        "index_failures_kind_check",
        &format!("failure_kind IN ({})", failure_kind::sql_in_list()),
    )
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        assert_eq!(INDEX_FAILURES, 42);
        assert_eq!(INDEX_FAILURES_NAME, "index_failures");
    }
}
