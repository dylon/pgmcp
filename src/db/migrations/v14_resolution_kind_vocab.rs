//! Migration step 14: `resolution_kind_vocab_v1`.
//!
//! Fixes a latent vocabulary drift that silently disabled the entire
//! symbol-reference resolution layer.
//!
//! `resolve_symbol_reference_targets` (`src/db/queries/symbols.rs`) writes the
//! confidence-graded bare-name tiers `bare_name_unique` / `bare_name_ambiguous`
//! (the graph-roadmap Phase 4.1 split). But the only definition of
//! `chk_symbol_refs_resolution_kind` (the `v2_shadow_asr` step) still allowed
//! the *pre-split* set `{exact_in_file, exact_via_import, bare_name_in_project,
//! external, unresolved}`. Because `v2_shadow_asr` is version-gated it never
//! re-runs, so the stale CHECK survived: Phase 3 of the resolver violated it,
//! and — since all four resolution phases share one transaction — the violation
//! rolled back **every** phase. The net effect on every project:
//! `resolution_kind`, `resolution_confidence`, and `target_symbol_id` stayed
//! NULL, the extraction watermark never advanced (full re-scan each run), and
//! every consumer filtering `resolution_kind IN (...)` (`effect_propagation`,
//! `change_impact_analysis`, `dead_code_reachability`, the reachability helpers
//! in `sema_helpers`) silently returned empty.
//!
//! This step re-issues the constraint from the closed
//! [`crate::parsing::resolution_kind::ResolutionKind`] enum (ADR-003 idiom:
//! TEXT + CHECK from `sql_in_list()` + a golden test pinning the set). It also
//! normalizes any straggler rows carrying a legacy value (e.g.
//! `bare_name_in_project`) to NULL so the `ADD CONSTRAINT` validation cannot
//! fail on historical data — the resolver repopulates them on its next run.
//!
//! Version-gated (runs once); every statement is idempotent, so a retried or
//! partial apply is safe.

use sqlx::PgPool;

use crate::parsing::resolution_kind;

pub(super) const RESOLUTION_KIND_VOCAB_V1: i32 = 14;
pub(super) const RESOLUTION_KIND_VOCAB_V1_NAME: &str = "resolution_kind_vocab_v1";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    let in_list = resolution_kind::sql_in_list();

    let mut tx = pool.begin().await?;
    // The CHECK validation re-scans `symbol_references`; on a large corpus that
    // can exceed the pooled connection's 30s `statement_timeout`. `SET LOCAL`
    // lifts it for this transaction only and reverts at commit.
    sqlx::query("SET LOCAL statement_timeout = 0")
        .execute(&mut *tx)
        .await?;

    // Defensive: clear any resolution_kind not in the new vocabulary (e.g. the
    // pre-split `bare_name_in_project`) before re-adding the constraint, so the
    // ADD CONSTRAINT below can't fail on a historical row. The resolver recomputes
    // these on its next run. Confidence is cleared in lockstep to stay consistent.
    let normalize = format!(
        "UPDATE symbol_references
            SET resolution_kind = NULL, resolution_confidence = NULL
          WHERE resolution_kind IS NOT NULL
            AND resolution_kind NOT IN ({in_list})"
    );
    sqlx::query(&normalize).execute(&mut *tx).await?;

    sqlx::query(
        "ALTER TABLE symbol_references DROP CONSTRAINT IF EXISTS chk_symbol_refs_resolution_kind",
    )
    .execute(&mut *tx)
    .await?;
    let add = format!(
        "ALTER TABLE symbol_references ADD CONSTRAINT chk_symbol_refs_resolution_kind
            CHECK (resolution_kind IS NULL OR resolution_kind IN ({in_list}))"
    );
    sqlx::query(&add).execute(&mut *tx).await?;

    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        // Pinning the constant — changing it is a schema-breaking event.
        assert_eq!(RESOLUTION_KIND_VOCAB_V1, 14);
        assert_eq!(RESOLUTION_KIND_VOCAB_V1_NAME, "resolution_kind_vocab_v1");
    }

    #[test]
    fn constraint_vocabulary_matches_enum() {
        // The CHECK is built from the enum's sql_in_list(); a drift here means
        // the resolver could write a value the constraint rejects (the exact bug
        // this step fixes). Pin the two together.
        let in_list = resolution_kind::sql_in_list();
        assert!(in_list.contains("'bare_name_unique'"));
        assert!(in_list.contains("'bare_name_ambiguous'"));
        assert!(!in_list.contains("bare_name_in_project"));
    }
}
