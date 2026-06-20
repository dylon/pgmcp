//! Regression: `run_migrations` and the runtime read/insert paths must
//! tolerate a post-`embed-cutover --drop-legacy` schema where the legacy 384d
//! `embedding` column is gone (BGE-M3 `embedding_v2` is canonical).
//!
//! On 2026-05-26 the daemon failed to boot with
//! `column "embedding" of relation "file_chunks" does not exist`:
//! `ensure_memory_v2_columns` ran `ALTER COLUMN embedding DROP NOT NULL` (and
//! the legacy HNSW creators ran `CREATE INDEX … (embedding …)`)
//! unconditionally on every boot, with no guard for the dropped column.

use pgmcp::config::VectorConfig;
use pgmcp_testing::require_test_db;
use sqlx::PgPool;

/// The four code-side tables whose legacy `embedding` column
/// `embed-cutover --drop-legacy` removes.
const LEGACY_TABLES: &[&str] = &[
    "file_chunks",
    "session_prompts",
    "git_commit_chunks",
    "software_pattern_chunks",
];

/// Drive the DB into the post-cutover state: stamp the BGE-M3 signature and
/// drop the legacy `embedding` column on all four tables, exactly as
/// `embed-cutover --promote` + `--drop-legacy` would.
async fn make_post_cutover(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO pgmcp_metadata (key, value)
         VALUES ('active_embedding_signature', 'bge-m3-v1')
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .execute(pool)
    .await
    .expect("stamp bge-m3-v1 signature");
    for t in LEGACY_TABLES {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "ALTER TABLE {t} DROP COLUMN IF EXISTS embedding"
        )))
        .execute(pool)
        .await
        .unwrap_or_else(|e| panic!("drop legacy embedding on {t}: {e}"));
    }
}

#[tokio::test]
async fn run_migrations_tolerates_dropped_legacy_embedding() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    make_post_cutover(&pool).await;

    // The regression assertion: re-running migrations must NOT error on the
    // post-cutover schema (this threw before the column-existence guards).
    pgmcp::db::migrations::run_migrations(&pool, &VectorConfig::default(), false)
        .await
        .expect("run_migrations must tolerate a post-cutover schema (first re-run)");
    // Idempotent: a second boot stays green too.
    pgmcp::db::migrations::run_migrations(&pool, &VectorConfig::default(), false)
        .await
        .expect("run_migrations must stay a no-op on repeated post-cutover boots");
}

#[tokio::test]
async fn run_migrations_hnsw_rebuild_skips_dropped_legacy_index() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    make_post_cutover(&pool).await;
    pgmcp::db::migrations::run_migrations(&pool, &VectorConfig::default(), false)
        .await
        .expect("baseline post-cutover migration");

    // Bump `m` so `needs_rebuild` is true → the legacy HNSW creators reach
    // their rebuild gate; with the column gone they must SKIP
    // `CREATE INDEX … (embedding …)`, not error. (The DROP NOT NULL guard
    // alone would not cover this path.)
    let mut cfg = VectorConfig::default();
    cfg.hnsw_m += 4;
    pgmcp::db::migrations::run_migrations(&pool, &cfg, false)
        .await
        .expect("HNSW rebuild must skip the dropped legacy index, not error");
}

#[tokio::test]
async fn post_cutover_runtime_paths_avoid_dropped_column() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    make_post_cutover(&pool).await;
    pgmcp::db::migrations::run_migrations(&pool, &VectorConfig::default(), false)
        .await
        .expect("post-cutover migration");

    // A 384d insert must surface the clear, BGE-M3/1024-only rejection
    // (not a raw `column "embedding" does not exist`). The dimension guard
    // in `insert_chunk` fires before any SQL, so an arbitrary file_id is fine.
    let err = pgmcp::db::queries::insert_chunk(&pool, 999_999, 0, "x", 1, 1, &vec![0.0f32; 384])
        .await
        .expect_err("384d insert must fail post-cutover (legacy column dropped)");
    let msg = format!("{err}");
    let msg_lc = msg.to_lowercase();
    assert!(
        msg_lc.contains("bge-m3") || msg_lc.contains("1024") || msg_lc.contains("embed-cutover"),
        "expected the BGE-M3/1024-only rejection message, got: {msg}"
    );

    // A read path that references the chunk-embedding column must use
    // `embedding_v2` — it would 42703 if it still named the dropped column.
    // Empty DB ⇒ empty result, but the SQL must execute without error.
    let pairs = pgmcp::db::queries::compare_chunks_within_file(&pool, 999_999, 0.5, 100)
        .await
        .expect("compare_chunks_within_file must read embedding_v2, not the dropped column");
    assert!(pairs.is_empty(), "no chunks seeded ⇒ no pairs");
}
