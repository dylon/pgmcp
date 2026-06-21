//! Migration step 53: make the context-tape working-set tables usable by the
//! live RLM path, and carry scratch-page bytes durably.
//!
//! ## Why (the keystone the tape integration needs)
//!
//! v51 created `working_set_pages` / `working_set_config` with a HARD foreign key
//! `session_key → orchestration_sessions(session_key)`. But the live recursive
//! language-model path (`run_rlm`) has NO orchestration session — it is keyed by a
//! `root_task_id` UUID. So any `PagingEngine` page-in from RLM would fail the FK
//! insert; that is precisely why the engine had zero production callers. We relax
//! the FK so `session_key` can hold EITHER a real `orchestration_sessions.session_key`
//! (CSM checkpoint path) OR the synthetic tree key `"rlm:{root_task_id}"` (RLM path).
//!
//! Cascade-on-session-delete is preserved for the CSM case by a BEFORE DELETE
//! trigger on `orchestration_sessions` (RLM rows have no matching session row, so
//! they are unaffected — they are reclaimed by `drop_tree` at run completion and
//! the `tape-store-reaper` cron).
//!
//! ## content column (scratch byte carriage)
//!
//! Corpus/observation/summary pages re-fetch their bytes from the read-only corpus
//! on resume, so they persist metadata only. `Scratch` pages (accumulator / REPL
//! output) have NO corpus source, so their bytes MUST be persisted to survive a
//! pause/resume — `working_set_pages.content` holds them (NULL for re-fetchable
//! pages). This is also what lets a dirty page's eviction write back real bytes
//! instead of the empty string.
//!
//! Additive + idempotent (`ADD COLUMN IF NOT EXISTS`, `DROP CONSTRAINT IF EXISTS`,
//! `CREATE OR REPLACE`, `DROP TRIGGER IF EXISTS`).

use sqlx::PgPool;

pub(super) const WORKING_SET_BYTES: i32 = 53;
pub(super) const WORKING_SET_BYTES_NAME: &str = "working_set_bytes";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    // ---- relax the hard FK so RLM trees (synthetic session_key) can persist ----
    for drop_fk in [
        "ALTER TABLE working_set_pages DROP CONSTRAINT IF EXISTS working_set_pages_session_key_fkey",
        "ALTER TABLE working_set_config DROP CONSTRAINT IF EXISTS working_set_config_session_key_fkey",
    ] {
        sqlx::query(drop_fk).execute(pool).await?;
    }

    // ---- scratch byte carriage ----
    sqlx::query("ALTER TABLE working_set_pages ADD COLUMN IF NOT EXISTS content TEXT")
        .execute(pool)
        .await?;

    // ---- tree_path index (RLM-tree cleanup + lookups by tree) ----
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_working_set_pages_tree
            ON working_set_pages (tree_path)",
    )
    .execute(pool)
    .await?;

    // ---- preserve cascade-on-session-delete for the CSM path via a trigger ----
    // (The dropped FK took its ON DELETE CASCADE with it; this restores the same
    // cleanup for real orchestration sessions without constraining session_key.)
    sqlx::query(
        "CREATE OR REPLACE FUNCTION working_set_cascade_on_session_delete()
         RETURNS trigger AS $$
         BEGIN
            DELETE FROM working_set_pages  WHERE session_key = OLD.session_key;
            DELETE FROM working_set_config WHERE session_key = OLD.session_key;
            RETURN OLD;
         END;
         $$ LANGUAGE plpgsql",
    )
    .execute(pool)
    .await?;
    sqlx::query("DROP TRIGGER IF EXISTS trg_working_set_cascade ON orchestration_sessions")
        .execute(pool)
        .await?;
    sqlx::query(
        "CREATE TRIGGER trg_working_set_cascade
            BEFORE DELETE ON orchestration_sessions
            FOR EACH ROW EXECUTE FUNCTION working_set_cascade_on_session_delete()",
    )
    .execute(pool)
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden version pin: v53 = (max existing migration v52) + 1.
    #[test]
    fn step_version_is_stable() {
        assert_eq!(WORKING_SET_BYTES, 53);
        assert_eq!(WORKING_SET_BYTES_NAME, "working_set_bytes");
    }
}
