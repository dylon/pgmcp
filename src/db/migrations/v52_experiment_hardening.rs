//! Migration step 52: experiment-subsystem **anti-tampering hardening** +
//! paired-corpus support.
//!
//! Adds the durable substrate for the generic experiment-API improvements
//! (requested by a consuming agent; the tools themselves are project-agnostic):
//!
//! 1. **Run-status lifecycle + CHECK** — `experiment_runs.status` becomes a
//!    closed vocabulary ([`ExperimentRunStatus`]: `pending`/`complete`/`finalized`/
//!    `invalid`/`superseded`). `experiment_decide` consumes ONLY runs whose status
//!    is *usable in a decision* (`complete`/`finalized`), so a nonconforming or
//!    operator-excluded run cannot silently skew a verdict. Existing rows are only
//!    ever `pending`/`complete`, so the CHECK validates cleanly.
//!
//! 2. **Status audit columns + immutable audit trail** — `status_reason` /
//!    `status_changed_by` / `status_changed_at` / `samples_digest` / `finalized_at`
//!    on the run, plus an append-only `experiment_run_status_audit` table. Every
//!    status change (finalize / invalidate / supersede) is recorded with a reason,
//!    an actor, and the samples digest at the time of the change — the anti-cherry-
//!    pick guardrail: invalidation is never silent, and a post-decision
//!    invalidation references the `decision_id` it re-opens.
//!
//! 3. **Tamper-evident samples digest** — `experiment_runs.samples_digest` holds a
//!    SHA-256 over the run's ordered raw samples, computed at finalize. A decision
//!    snapshots the digest; if the samples are later mutated, the digest mismatch
//!    is detectable. (Computed in Rust; this migration only provisions the column.)
//!
//! 4. **Paired-corpus 2×2 counts** — `experiment_paired_binary` stores
//!    `{both_correct, control_only, treatment_only, both_wrong}` for a
//!    `(experiment, hypothesis, metric)`, the correct compact representation for
//!    classification/recall benchmarks where Welch's t-test is inappropriate and
//!    McNemar's test (or the exact binomial) is correct.
//!
//! ## Boundary
//!
//! This is the EXPERIMENT subsystem only. Experiment outcomes promote to *memory*
//! (default-OFF, server-computed) — they NEVER cross the work-item tracker's
//! `→ verified` boundary (that stays CI-only; see the 2026-06-20 loophole revert).
//!
//! Additive + `IF NOT EXISTS` / guarded `ADD CONSTRAINT`, so idempotent and
//! version-gated by `apply_step`. Closed vocab follows the ADR-003 idiom.

use sqlx::PgPool;

use crate::experiment::vocab::ExperimentRunStatus;

pub(super) const EXPERIMENT_HARDENING: i32 = 52;
pub(super) const EXPERIMENT_HARDENING_NAME: &str = "experiment_hardening";

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    // ---- 1. run-status closed vocabulary (CHECK) ---------------------------
    // Existing rows are only ever 'pending'/'complete' (both in the vocab), so the
    // constraint validates cleanly. Guarded so re-running is a no-op.
    let add_status_check = format!(
        "DO $$
         BEGIN
            IF NOT EXISTS (
                SELECT 1 FROM pg_constraint WHERE conname = 'experiment_runs_status_check'
            ) THEN
                ALTER TABLE experiment_runs
                    ADD CONSTRAINT experiment_runs_status_check
                    CHECK (status IN ({status}));
            END IF;
         END $$;",
        status = ExperimentRunStatus::sql_in_list(),
    );
    sqlx::query(sqlx::AssertSqlSafe(add_status_check.as_str()))
        .execute(pool)
        .await?;

    // ---- 2. status audit columns on the run --------------------------------
    for col in [
        "ALTER TABLE experiment_runs ADD COLUMN IF NOT EXISTS status_reason TEXT",
        "ALTER TABLE experiment_runs ADD COLUMN IF NOT EXISTS status_changed_by TEXT",
        "ALTER TABLE experiment_runs ADD COLUMN IF NOT EXISTS status_changed_at TIMESTAMPTZ",
        "ALTER TABLE experiment_runs ADD COLUMN IF NOT EXISTS samples_digest TEXT",
        "ALTER TABLE experiment_runs ADD COLUMN IF NOT EXISTS finalized_at TIMESTAMPTZ",
    ] {
        sqlx::query(col).execute(pool).await?;
    }

    // ---- 3. immutable status audit trail -----------------------------------
    // Append-only: every run-status transition is recorded with a reason + actor +
    // the samples digest at the time. `decision_id` is set when a post-decision
    // invalidation re-opens a rendered decision (so excluding data after the fact
    // is auditable and forces the decision to be re-evaluated, never silent).
    let audit = format!(
        "CREATE TABLE IF NOT EXISTS experiment_run_status_audit (
            id              BIGSERIAL PRIMARY KEY,
            run_id          UUID NOT NULL REFERENCES experiment_runs(id) ON DELETE CASCADE,
            old_status      TEXT,
            new_status      TEXT NOT NULL CHECK (new_status IN ({status})),
            reason          TEXT NOT NULL,
            changed_by      TEXT NOT NULL,
            decision_id     BIGINT REFERENCES experiment_results(id) ON DELETE SET NULL,
            samples_digest  TEXT,
            changed_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
        status = ExperimentRunStatus::sql_in_list(),
    );
    sqlx::query(sqlx::AssertSqlSafe(audit.as_str()))
        .execute(pool)
        .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_experiment_run_status_audit_run
            ON experiment_run_status_audit (run_id, changed_at)",
    )
    .execute(pool)
    .await?;

    // ---- 4. paired-corpus 2×2 counts ---------------------------------------
    // One 2×2 per (experiment, hypothesis, metric) — upserted. Non-negative
    // counts; the McNemar / exact-binomial decision reads these directly.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS experiment_paired_binary (
            id               BIGSERIAL PRIMARY KEY,
            experiment_id    BIGINT NOT NULL REFERENCES experiments(id) ON DELETE CASCADE,
            hypothesis_id    BIGINT REFERENCES experiment_hypotheses(id) ON DELETE SET NULL,
            metric_name      TEXT NOT NULL,
            control_run_id   UUID REFERENCES experiment_runs(id) ON DELETE SET NULL,
            treatment_run_id UUID REFERENCES experiment_runs(id) ON DELETE SET NULL,
            both_correct     BIGINT NOT NULL CHECK (both_correct >= 0),
            control_only     BIGINT NOT NULL CHECK (control_only >= 0),
            treatment_only   BIGINT NOT NULL CHECK (treatment_only >= 0),
            both_wrong       BIGINT NOT NULL CHECK (both_wrong >= 0),
            source           TEXT,
            detail           JSONB NOT NULL DEFAULT '{}'::jsonb,
            created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            updated_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            UNIQUE (experiment_id, hypothesis_id, metric_name)
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_experiment_paired_binary_exp
            ON experiment_paired_binary (experiment_id)",
    )
    .execute(pool)
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden version pin: v52 is (max existing migration v51) + 1. A later
    /// migration MUST take 53+; bumping this silently would desync the
    /// `apply_step` ordering.
    #[test]
    fn step_version_is_stable() {
        assert_eq!(EXPERIMENT_HARDENING, 52);
        assert_eq!(EXPERIMENT_HARDENING_NAME, "experiment_hardening");
    }

    /// The status CHECK lists are sourced from the Rust enum, so the constraint
    /// and the closed vocabulary cannot drift (ADR-003).
    #[test]
    fn ddl_sources_status_check_from_enum() {
        let list = ExperimentRunStatus::sql_in_list();
        assert!(list.contains("'pending'"));
        assert!(list.contains("'complete'"));
        assert!(list.contains("'finalized'"));
        assert!(list.contains("'invalid'"));
        assert!(list.contains("'superseded'"));
        // The decision-usability gate is the anti-tamper rule the tools enforce.
        assert!(ExperimentRunStatus::Complete.usable_in_decision());
        assert!(ExperimentRunStatus::Finalized.usable_in_decision());
        assert!(!ExperimentRunStatus::Invalid.usable_in_decision());
        assert!(!ExperimentRunStatus::Superseded.usable_in_decision());
        assert!(!ExperimentRunStatus::Pending.usable_in_decision());
    }
}
