//! RC1 regression — vocabulary↔catalog parity.
//!
//! The every-boot `reconcile_vocabulary_catalogs` (in `pgmcp::db::migrations`)
//! must keep `effect_catalog ⊇ SEED_EFFECTS` and `type_tag_catalog ⊇
//! SEED_TYPE_TAGS`, and must HEAL drift on an already-migrated database. This is
//! the catalog-superset invariant ADR-003 mandated but never had a test for —
//! its absence let the v21 concurrency effects (`await_point`, `lock_acquire`,
//! …) drift out of `effect_catalog`, so the `symbol_effects_effect_fkey` FK
//! rejected every symbol carrying one and the whole file was skipped.
//!
//! Real-DB test: `require_test_db!` runs `run_migrations` on a fresh template,
//! so these assertions exercise the actual fresh-install + reconcile paths.

use pgmcp::config::VectorConfig;
use pgmcp::parsing::type_tags::vocabulary::{SEED_EFFECTS, SEED_TYPE_TAGS};
use pgmcp_testing::require_test_db;
use sqlx::PgPool;

/// Names in `seed` that are absent from catalog `table` (the catalog-⊇-seed gap).
async fn missing_from_catalog(pool: &PgPool, table: &str, seed: &[String]) -> Vec<String> {
    let sql = format!(
        "SELECT v.name FROM UNNEST($1::text[]) AS v(name)
         WHERE NOT EXISTS (SELECT 1 FROM {table} c WHERE c.name = v.name)"
    );
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql.as_str()))
        .bind(seed.to_vec())
        .fetch_all(pool)
        .await
        .expect("anti-join query must succeed")
}

fn effect_names() -> Vec<String> {
    SEED_EFFECTS.iter().map(|t| t.name.to_string()).collect()
}

fn type_tag_names() -> Vec<String> {
    SEED_TYPE_TAGS.iter().map(|t| t.name.to_string()).collect()
}

#[tokio::test]
async fn catalogs_are_supersets_of_the_rust_vocabulary() {
    let db = require_test_db!();
    let pool = db.pool().clone();

    let missing_effects = missing_from_catalog(&pool, "effect_catalog", &effect_names()).await;
    assert!(
        missing_effects.is_empty(),
        "effect_catalog is missing {} vocabulary effects: {:?} — symbol_effects \
         inserts for these would FK-skip the whole file",
        missing_effects.len(),
        missing_effects
    );

    let missing_tags = missing_from_catalog(&pool, "type_tag_catalog", &type_tag_names()).await;
    assert!(
        missing_tags.is_empty(),
        "type_tag_catalog is missing {} vocabulary type tags: {:?}",
        missing_tags.len(),
        missing_tags
    );
}

#[tokio::test]
async fn reconcile_heals_effect_catalog_drift() {
    let db = require_test_db!();
    let pool = db.pool().clone();

    // Reproduce the production drift: drop the v21 concurrency effects from the
    // catalog. (The FK on `symbol_effects.effect` is ON DELETE RESTRICT, but the
    // fresh template has no `symbol_effects` rows referencing them, so the
    // delete succeeds.)
    let concurrency: Vec<String> = [
        "await_point",
        "channel_select",
        "lock_acquire",
        "lock_release",
        "thread_spawn",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let deleted = sqlx::query("DELETE FROM effect_catalog WHERE name = ANY($1::text[])")
        .bind(&concurrency)
        .execute(&pool)
        .await
        .expect("delete concurrency effects");
    assert_eq!(
        deleted.rows_affected(),
        5,
        "the 5 concurrency effects should have been present to delete"
    );

    // Re-running migrations must re-seed them via reconcile_vocabulary_catalogs
    // (idempotent + unconditional), healing the drift on an existing DB.
    pgmcp::db::migrations::run_migrations(&pool, &VectorConfig::default(), false)
        .await
        .expect("run_migrations must heal catalog drift");

    let still_missing = missing_from_catalog(&pool, "effect_catalog", &effect_names()).await;
    assert!(
        still_missing.is_empty(),
        "the every-boot reconcile must heal deleted effects; still missing: {:?}",
        still_missing
    );
}
