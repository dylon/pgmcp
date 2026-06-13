//! Migration step 36: convert the experiment subsystem's 5 native PostgreSQL
//! `ENUM` types to the ADR-003 TEXT + CHECK idiom.
//!
//! Before this step, `experiments.{kind,status}`,
//! `experiment_hypotheses.{predicted_direction,verdict}`,
//! `experiment_runs.arm_kind`, and `experiment_results.verdict` were native
//! enums (`experiment_kind`, `experiment_status`, `hypothesis_verdict`,
//! `experiment_arm_kind`, `effect_direction`). A native enum forces a
//! `col = $n::enumtype` cast at every comparison; forgetting it yields the
//! `operator does not exist: experiment_kind = text` class of runtime failure.
//!
//! This converts every such column to `TEXT`, installs a `CHECK` built from the
//! closed Rust enums in [`crate::experiment::vocab`] (the single source of
//! truth), and drops the now-unused enum types — making that bug class
//! impossible. `enumlabel::text` preserves every existing row's value verbatim.
//!
//! Idempotent: each column is converted only while still `USER-DEFINED`
//! (a re-run, or a fresh install whose `ensure_experiment_tables` already
//! created the column as TEXT, skips the rewrite); CHECKs are dropped-if-exists
//! then re-added; type drops are `DROP TYPE IF EXISTS`. The whole step runs in one
//! transaction with `statement_timeout = 0`, since the `ALTER … TYPE` rewrite
//! and the CHECK validation can exceed the pooled timeout on a populated DB.

use sqlx::PgPool;

use crate::experiment::vocab::{
    EffectDirection, ExperimentArmKind, ExperimentKind, ExperimentStatus, HypothesisVerdict,
};

pub(super) const EXPERIMENT_ENUM_TO_TEXT: i32 = 36;
pub(super) const EXPERIMENT_ENUM_TO_TEXT_NAME: &str = "experiment_enum_to_text";

/// One (table, column, optional-default) conversion target. All identifiers are
/// compile-time literals (no user input), so the `format!`-built DDL is safe.
struct Col {
    table: &'static str,
    column: &'static str,
    default: Option<&'static str>,
}

pub(super) async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout = 0")
        .execute(&mut *tx)
        .await?;

    let cols = [
        Col {
            table: "experiments",
            column: "kind",
            default: Some("other"),
        },
        Col {
            table: "experiments",
            column: "status",
            default: Some("open"),
        },
        Col {
            table: "experiment_hypotheses",
            column: "predicted_direction",
            default: Some("either"),
        },
        Col {
            table: "experiment_hypotheses",
            column: "verdict",
            default: Some("pending"),
        },
        Col {
            table: "experiment_runs",
            column: "arm_kind",
            default: None,
        },
        Col {
            table: "experiment_results",
            column: "verdict",
            default: None,
        },
    ];

    for c in cols {
        // Only convert while the column is still the native enum
        // ('USER-DEFINED'); 'text' means it is already converted (re-run) or was
        // created as TEXT on a fresh install — skip the rewrite, still ensure the
        // CHECK below.
        let data_type: Option<String> = sqlx::query_scalar::<_, String>(
            "SELECT data_type FROM information_schema.columns
             WHERE table_name = $1 AND column_name = $2",
        )
        .bind(c.table)
        .bind(c.column)
        .fetch_optional(&mut *tx)
        .await?;

        if data_type.as_deref() == Some("USER-DEFINED") {
            // 1. Drop the typed default (e.g. 'other'::experiment_kind), which
            //    blocks the type change.
            sqlx::query(&format!(
                "ALTER TABLE {} ALTER COLUMN {} DROP DEFAULT",
                c.table, c.column
            ))
            .execute(&mut *tx)
            .await?;
            // 2. Convert enum → text, preserving each row's label string.
            sqlx::query(&format!(
                "ALTER TABLE {t} ALTER COLUMN {col} TYPE text USING {col}::text",
                t = c.table,
                col = c.column
            ))
            .execute(&mut *tx)
            .await?;
            // 3. Re-add the TEXT default for the columns that had one.
            if let Some(def) = c.default {
                sqlx::query(&format!(
                    "ALTER TABLE {} ALTER COLUMN {} SET DEFAULT '{}'",
                    c.table, c.column, def
                ))
                .execute(&mut *tx)
                .await?;
            }
        }
    }

    // CHECK constraints, built from the Rust enums. Done after all columns are
    // TEXT so validation runs against converted data. The constraint name is
    // PostgreSQL's deterministic `<table>_<column>_check`, so a future
    // vocabulary-widening migration (ADR-003 idiom, cf. v33/v35) can find and
    // re-issue it.
    let checks = [
        ("experiments", "kind", ExperimentKind::sql_in_list()),
        ("experiments", "status", ExperimentStatus::sql_in_list()),
        (
            "experiment_hypotheses",
            "predicted_direction",
            EffectDirection::sql_in_list(),
        ),
        (
            "experiment_hypotheses",
            "verdict",
            HypothesisVerdict::sql_in_list(),
        ),
        (
            "experiment_runs",
            "arm_kind",
            ExperimentArmKind::sql_in_list(),
        ),
        (
            "experiment_results",
            "verdict",
            HypothesisVerdict::sql_in_list(),
        ),
    ];
    for (table, column, in_list) in checks {
        let cname = format!("{table}_{column}_check");
        sqlx::query(&format!(
            "ALTER TABLE {table} DROP CONSTRAINT IF EXISTS {cname}"
        ))
        .execute(&mut *tx)
        .await?;
        sqlx::query(&format!(
            "ALTER TABLE {table} ADD CONSTRAINT {cname} CHECK ({column} IN ({in_list}))"
        ))
        .execute(&mut *tx)
        .await?;
    }

    // Drop the now-unused native enum types LAST: a type cannot be dropped while
    // any column is still typed by it, so this must follow every `ALTER … TYPE
    // text` above. No CASCADE — a stray dependant should fail loudly, not be
    // silently dropped. `IF EXISTS` makes a re-run a no-op.
    for ty in [
        "experiment_kind",
        "experiment_status",
        "hypothesis_verdict",
        "experiment_arm_kind",
        "effect_direction",
    ] {
        sqlx::query(&format!("DROP TYPE IF EXISTS {ty}"))
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_version_is_stable() {
        assert_eq!(EXPERIMENT_ENUM_TO_TEXT, 36);
        assert_eq!(EXPERIMENT_ENUM_TO_TEXT_NAME, "experiment_enum_to_text");
    }

    #[test]
    fn check_vocabularies_match_enums() {
        assert!(ExperimentKind::sql_in_list().contains("'optimization'"));
        assert!(ExperimentStatus::sql_in_list().contains("'measuring'"));
        assert!(HypothesisVerdict::sql_in_list().contains("'inconclusive'"));
        assert!(ExperimentArmKind::sql_in_list().contains("'baseline'"));
        assert!(EffectDirection::sql_in_list().contains("'increase'"));
    }
}
