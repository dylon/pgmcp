//! Migration step 15: `symbol_effect_history_v1` — the temporal effect-drift
//! ledger.
//!
//! Shadow-ASR (`v2_shadow_asr`) records each symbol's *current* effect set in
//! `symbol_effects`, but that table is rewritten (delete-then-insert) on every
//! symbol-extraction run, so it has no memory of *when* a symbol gained or lost
//! an effect. This append-only ledger captures that drift: the symbol-extraction
//! cron diffs each file's freshly-extracted effect set against the prior set and
//! records `gained` / `lost` transitions here, keyed by the stable
//! `(file_id, symbol_kind, symbol_name)` identity (line numbers move; the
//! kind+name identity is what a human means by "this function").
//!
//! Rows are immutable history — `effect` is plain `TEXT` with **no** FK to
//! `effect_catalog`, so a later catalog edit never rewrites or deletes the
//! historical record. The only lifecycle tie is `ON DELETE CASCADE` on
//! `file_id`: if a file is removed from the index its drift history goes with it.
//!
//! Powers the `effect_drift` MCP tool ("functions that recently became
//! `unsafe` / `async` / `blocking_io`") and is the source for an optional
//! `gained_effect` temporal edge in a future graph-RAG pass.
//!
//! Version-gated (runs once); every statement is `IF NOT EXISTS` / idempotent.

use sqlx::PgPool;

pub(super) const SYMBOL_EFFECT_HISTORY_V1: i32 = 15;
pub(super) const SYMBOL_EFFECT_HISTORY_V1_NAME: &str = "symbol_effect_history_v1";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS symbol_effect_history (
            id           BIGSERIAL PRIMARY KEY,
            file_id      BIGINT NOT NULL REFERENCES indexed_files(id) ON DELETE CASCADE,
            symbol_kind  TEXT NOT NULL,
            symbol_name  TEXT NOT NULL,
            effect       TEXT NOT NULL,
            change       TEXT NOT NULL CHECK (change IN ('gained', 'lost')),
            observed_at  TIMESTAMPTZ NOT NULL DEFAULT now()
        )",
    )
    .execute(pool)
    .await?;

    // Query paths: by project/file (join indexed_files on file_id), by effect,
    // by recency. A composite (effect, observed_at DESC) serves the common
    // "what recently became unsafe" query; the file_id index serves cascade +
    // per-project rollups.
    let indexes = [
        "CREATE INDEX IF NOT EXISTS idx_symbol_effect_history_file
            ON symbol_effect_history (file_id)",
        "CREATE INDEX IF NOT EXISTS idx_symbol_effect_history_effect_time
            ON symbol_effect_history (effect, observed_at DESC)",
        "CREATE INDEX IF NOT EXISTS idx_symbol_effect_history_observed
            ON symbol_effect_history (observed_at DESC)",
    ];
    for stmt in indexes {
        sqlx::query(stmt).execute(pool).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        // Pinning the constant — changing it is a schema-breaking event.
        assert_eq!(SYMBOL_EFFECT_HISTORY_V1, 15);
        assert_eq!(SYMBOL_EFFECT_HISTORY_V1_NAME, "symbol_effect_history_v1");
    }
}
