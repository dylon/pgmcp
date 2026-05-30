//! Migration step 16: `work_item_assignee_v1` — durable ownership intent.
//!
//! Phase 2 of the tracker roadmap (`~/.claude/plans/how-extensive-is-the-zazzy-galaxy.md`,
//! "Tracker ergonomics & next-action") adds a *durable* assignee axis to the
//! `work_items` spine, distinct from the existing *ephemeral* `claimed_by` lease
//! (v5 collaboration layer):
//!
//! - `claimed_by` is a CAS lease that auto-expires (crash-safe) and is cleared
//!   on release / handoff / expiry — it answers "who is actively executing this
//!   right now?".
//! - `assignee` is set explicitly via `work_item_assign` and is **never
//!   auto-cleared** — it answers "who *owns* this work?" (the `my-work`
//!   smart-view filters on it).
//!
//! All three columns are nullable free-text (an agent id, like `claimed_by`) —
//! no CHECK, so any agent / human identifier round-trips. `ADD COLUMN IF NOT
//! EXISTS` with no default is an instant metadata-only change (no table rewrite,
//! no backfill); existing rows keep `assignee = NULL` (unassigned). The partial
//! index serves the `my-work` queue (assignee + priority) and pays nothing for
//! the common unassigned rows.
//!
//! Version-gated (runs once); every statement is `IF NOT EXISTS` / idempotent.

use sqlx::PgPool;

pub(super) const WORK_ITEM_ASSIGNEE_V1: i32 = 16;
pub(super) const WORK_ITEM_ASSIGNEE_V1_NAME: &str = "work_item_assignee_v1";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    // 1. The durable ownership column + its provenance pair, all nullable
    //    free-text (no CHECK — a free-text agent id, exactly like `claimed_by`).
    sqlx::query("ALTER TABLE work_items ADD COLUMN IF NOT EXISTS assignee TEXT")
        .execute(pool)
        .await?;
    sqlx::query("ALTER TABLE work_items ADD COLUMN IF NOT EXISTS assigned_at TIMESTAMPTZ")
        .execute(pool)
        .await?;
    sqlx::query("ALTER TABLE work_items ADD COLUMN IF NOT EXISTS assigned_by TEXT")
        .execute(pool)
        .await?;

    // 2. Document the durable-vs-lease distinction at the schema level so a
    //    future reader does not confuse `assignee` with `claimed_by`.
    sqlx::query(
        "COMMENT ON COLUMN work_items.assignee IS \
         'Durable ownership intent, distinct from the ephemeral claimed_by lease; \
set via work_item_assign, never auto-cleared.'",
    )
    .execute(pool)
    .await?;

    // 3. Partial index for the `my-work` queue: ordered by priority within an
    //    assignee. Partial (`WHERE assignee IS NOT NULL`) so the common
    //    unassigned rows pay no storage — the same discipline as the v12
    //    `idx_work_items_severity` partial index.
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_work_items_assignee \
            ON work_items(assignee, priority DESC) WHERE assignee IS NOT NULL",
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
        assert_eq!(WORK_ITEM_ASSIGNEE_V1, 16);
        assert_eq!(WORK_ITEM_ASSIGNEE_V1_NAME, "work_item_assignee_v1");
    }
}
