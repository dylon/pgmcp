//! Real-DB parity for the experiment vocabularies (ADR-003 / the
//! `v36_experiment_enum_to_text` migration).
//!
//! After v36 the five native enum types (`experiment_kind`, `experiment_status`,
//! `hypothesis_verdict`, `experiment_arm_kind`, `effect_direction`) must be gone,
//! their six columns must be `TEXT`, and the CHECK constraints must enforce
//! exactly the closed Rust vocabularies in [`pgmcp::experiment::vocab`]. Pinning
//! this end-to-end makes the `operator does not exist: experiment_kind = text`
//! bug class — and any drift between the Rust enum and the DB CHECK —
//! impossible.
//!
//! `require_test_db!` runs `run_migrations` on a fresh template, so these
//! assertions exercise the actual fresh-install + v36 path.

use pgmcp::config::VectorConfig;
use pgmcp::experiment::vocab::{
    EffectDirection, ExperimentArmKind, ExperimentKind, ExperimentStatus, HypothesisVerdict,
};
use pgmcp_testing::require_test_db;
use sqlx::PgPool;
use uuid::Uuid;

async fn type_exists(pool: &PgPool, name: &str) -> bool {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_type WHERE typname = $1)")
        .bind(name)
        .fetch_one(pool)
        .await
        .expect("pg_type probe")
}

async fn column_type(pool: &PgPool, table: &str, column: &str) -> Option<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT data_type FROM information_schema.columns
         WHERE table_name = $1 AND column_name = $2",
    )
    .bind(table)
    .bind(column)
    .fetch_optional(pool)
    .await
    .expect("information_schema probe")
}

#[tokio::test]
async fn native_enum_types_are_dropped() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    for ty in [
        "experiment_kind",
        "experiment_status",
        "hypothesis_verdict",
        "experiment_arm_kind",
        "effect_direction",
    ] {
        assert!(
            !type_exists(&pool, ty).await,
            "native enum type `{ty}` must be dropped by v36 (TEXT+CHECK per ADR-003)"
        );
    }
}

#[tokio::test]
async fn enum_columns_are_text() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    for (table, column) in [
        ("experiments", "kind"),
        ("experiments", "status"),
        ("experiment_hypotheses", "predicted_direction"),
        ("experiment_hypotheses", "verdict"),
        ("experiment_runs", "arm_kind"),
        ("experiment_results", "verdict"),
    ] {
        assert_eq!(
            column_type(&pool, table, column).await.as_deref(),
            Some("text"),
            "{table}.{column} must be TEXT after v36"
        );
    }
}

#[tokio::test]
async fn check_enforces_the_rust_vocabulary() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let suffix = Uuid::new_v4().simple();

    // Every valid ExperimentKind inserts; an out-of-vocabulary value is rejected
    // by experiments_kind_check — the Rust enum is the single source of truth.
    for k in ExperimentKind::ALL {
        let slug = format!("vocab-ok-{}-{suffix}", k.as_str());
        let r = sqlx::query(
            "INSERT INTO experiments (slug, title, question, kind) VALUES ($1, 't', 'q', $2)",
        )
        .bind(&slug)
        .bind(k.as_str())
        .execute(&pool)
        .await;
        assert!(
            r.is_ok(),
            "valid kind {:?} must insert: {:?}",
            k.as_str(),
            r.err()
        );
    }

    let bad = sqlx::query(
        "INSERT INTO experiments (slug, title, question, kind) VALUES ($1, 't', 'q', $2)",
    )
    .bind(format!("vocab-bad-{suffix}"))
    .bind("not_a_real_kind")
    .execute(&pool)
    .await;
    assert!(
        bad.is_err(),
        "an out-of-vocabulary kind must be rejected by experiments_kind_check"
    );

    // The other four CHECKs exist and enforce a representative value from each
    // enum (the constraint definition contains the quoted value).
    for (cname, value) in [
        (
            "experiments_status_check",
            ExperimentStatus::Measuring.as_str(),
        ),
        (
            "experiment_hypotheses_verdict_check",
            HypothesisVerdict::Inconclusive.as_str(),
        ),
        (
            "experiment_runs_arm_kind_check",
            ExperimentArmKind::Baseline.as_str(),
        ),
        (
            "experiment_hypotheses_predicted_direction_check",
            EffectDirection::Increase.as_str(),
        ),
    ] {
        let def: Option<String> = sqlx::query_scalar(
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = $1",
        )
        .bind(cname)
        .fetch_optional(&pool)
        .await
        .expect("pg_constraint probe");
        let def = def.unwrap_or_else(|| panic!("CHECK {cname} must exist after v36"));
        assert!(
            def.contains(value),
            "CHECK {cname} must enforce '{value}'; got: {def}"
        );
    }
}

/// Exercise v36's CONVERSION branch (the path that runs on an EXISTING enum
/// install at daemon restart, which the fresh-install parity tests above never
/// hit): revert `experiments.kind` to the native `experiment_kind` enum, insert
/// a real row, un-record v36, and re-run migrations. v36 must DROP DEFAULT →
/// `ALTER … TYPE text` → SET DEFAULT → install CHECK → DROP TYPE, preserving the
/// row's label verbatim.
#[tokio::test]
async fn v36_converts_an_existing_native_enum_column_in_place() {
    let db = require_test_db!();
    let pool = db.pool().clone();

    // Simulate the pre-v36 shape for one representative column.
    for stmt in [
        "ALTER TABLE experiments DROP CONSTRAINT IF EXISTS experiments_kind_check",
        "ALTER TABLE experiments ALTER COLUMN kind DROP DEFAULT",
        "CREATE TYPE experiment_kind AS ENUM \
         ('optimization','feature_refactor','feature_addition','bugfix','investigation','other')",
        "ALTER TABLE experiments ALTER COLUMN kind TYPE experiment_kind USING kind::experiment_kind",
        "ALTER TABLE experiments ALTER COLUMN kind SET DEFAULT 'other'::experiment_kind",
    ] {
        sqlx::query(stmt)
            .execute(&pool)
            .await
            .expect("revert kind to native enum");
    }
    assert_eq!(
        column_type(&pool, "experiments", "kind").await.as_deref(),
        Some("USER-DEFINED"),
        "precondition: kind is the native enum again"
    );

    let suffix = Uuid::new_v4().simple();
    let slug = format!("v36-conv-{suffix}");
    sqlx::query(
        "INSERT INTO experiments (slug, title, question, kind) VALUES ($1, 't', 'q', 'bugfix')",
    )
    .bind(&slug)
    .execute(&pool)
    .await
    .expect("insert enum-typed row");

    // Un-record v36 so re-running migrations fires its conversion branch.
    sqlx::query("DELETE FROM pgmcp_schema_versions WHERE version = 36")
        .execute(&pool)
        .await
        .expect("un-record v36");
    pgmcp::db::migrations::run_migrations(&pool, &VectorConfig::default())
        .await
        .expect("re-run migrations must convert the enum column");

    assert_eq!(
        column_type(&pool, "experiments", "kind").await.as_deref(),
        Some("text"),
        "v36 must convert the native enum column to TEXT"
    );
    assert!(
        !type_exists(&pool, "experiment_kind").await,
        "v36 must drop experiment_kind after converting its last column"
    );
    let kept: String = sqlx::query_scalar("SELECT kind FROM experiments WHERE slug = $1")
        .bind(&slug)
        .fetch_one(&pool)
        .await
        .expect("row survives conversion");
    assert_eq!(
        kept, "bugfix",
        "the enum label must be preserved verbatim as TEXT"
    );
    let bad = sqlx::query("UPDATE experiments SET kind = 'not_a_kind' WHERE slug = $1")
        .bind(&slug)
        .execute(&pool)
        .await;
    assert!(
        bad.is_err(),
        "the reinstalled CHECK must reject out-of-vocab values"
    );
}
