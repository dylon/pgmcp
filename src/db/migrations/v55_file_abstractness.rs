//! Migration step 55: per-file Abstractness on `file_metrics` (first-class
//! Robert C. Martin package-metric `A`).
//!
//! ## Why
//!
//! Martin's package metrics use Abstractness `A = abstract_types / total_types`,
//! Instability `I = Ce/(Ca+Ce)`, and main-sequence distance `D* = |A + I − 1|`.
//! Until now `A` was computed only transiently inside `coupling_cohesion_report`
//! (from file CONTENT via a regex) and never persisted; the graph-analysis cron
//! fed a hardcoded `abstractness = 0.0` into the v48 rollup, so
//! `module_metrics.abstractness` / `project_metrics.avg_abstractness` were always
//! `0` and the persisted `distance_from_main_sequence` was the placeholder
//! `instability` (= `I`) rather than `|A + I − 1|` — an inverted distance on
//! Rust, which `architecture_quality_score = 1 − avg_distance` then propagated.
//!
//! This migration gives abstractness a first-class home at the FILE grain so the
//! cron can compute it **content-independently** from `file_symbols` (a file is
//! abstract iff it declares ≥1 `trait`/`interface`) and roll it up. `is_abstract`
//! is the canonical Martin per-file indicator (a module's `A` is the mean of
//! these booleans); the two counts make `is_abstract` a derived value
//! (`abstract_type_count > 0`), preserve a true per-file ratio, and give the
//! scoring layer a "project declares no types at all" degeneracy signal — so no
//! second migration is needed when a finer ratio is wanted.
//!
//! The v48 `module_metrics`/`project_metrics` abstractness columns ALREADY exist
//! and are already written by `persist_project_rollup`; this migration does NOT
//! re-add them. It does close the one v48 gap — `hier_group_metrics` (the
//! group/workspace tier) lacked `avg_abstractness` — so abstractness is
//! first-class at all four hierarchy tiers.
//!
//! `is_abstract` is a BOOLEAN, not a closed string vocabulary, so the ADR-003
//! `TEXT + CHECK + sql_in_list()` idiom does not apply. Additive + idempotent
//! (`ADD COLUMN IF NOT EXISTS`); existing rows backfill to FALSE/0 (a constant
//! default — metadata-only, no table rewrite) and are corrected on the next
//! graph-analysis pass.

use sqlx::PgPool;

pub(super) const FILE_ABSTRACTNESS: i32 = 55;
pub(super) const FILE_ABSTRACTNESS_NAME: &str = "file_abstractness";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    // Per-file abstractness at the file grain.
    sqlx::query(
        "ALTER TABLE file_metrics
            ADD COLUMN IF NOT EXISTS is_abstract         BOOLEAN NOT NULL DEFAULT FALSE,
            ADD COLUMN IF NOT EXISTS abstract_type_count INTEGER NOT NULL DEFAULT 0,
            ADD COLUMN IF NOT EXISTS concrete_type_count INTEGER NOT NULL DEFAULT 0",
    )
    .execute(pool)
    .await?;

    // Close the v48 group/workspace-tier gap so abstractness rolls up to every
    // hierarchy level (file → module → project → group/workspace).
    sqlx::query(
        "ALTER TABLE hier_group_metrics
            ADD COLUMN IF NOT EXISTS avg_abstractness DOUBLE PRECISION NOT NULL DEFAULT 0",
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
        assert_eq!(FILE_ABSTRACTNESS, 55);
        assert_eq!(FILE_ABSTRACTNESS_NAME, "file_abstractness");
    }
}
