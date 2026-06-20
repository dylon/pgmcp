//! Regression: a `memory_unified` matview DEFINITION change must NOT trigger the
//! (potentially multi-minute) DROP + HNSW rebuild on the daemon's startup
//! critical path.
//!
//! The 2026-06-19 incident: vector-seeding the `topic` node flipped the
//! view-definition hash, so on startup `run_migrations` rebuilt the 765k-vector
//! HNSW index on `memory_unified_nodes` *before* signaling systemd `READY`. That
//! rebuild exceeded `TimeoutStartSec=300`; systemd killed it and
//! `Restart=on-failure` crash-looped the service forever.
//!
//! The fix (`defer_unified_rebuild`): with `defer = true` (the daemon path),
//! `run_migrations` only ENSURES the matviews EXIST and leaves any hash-change
//! rebuild to a post-`READY` background task. This test pins that contract via
//! the public migration API.

use pgmcp::config::VectorConfig;
use pgmcp_testing::require_test_db;

const HASH_KEY: &str = "memory_unified_views_def_hash";

async fn stored_hash(pool: &sqlx::PgPool) -> Option<String> {
    sqlx::query_scalar("SELECT value FROM pgmcp_metadata WHERE key = $1")
        .bind(HASH_KEY)
        .fetch_optional(pool)
        .await
        .expect("read view-definition hash")
}

#[tokio::test]
async fn defer_skips_matview_rebuild_on_definition_change() {
    let db = require_test_db!();
    let pool = db.pool().clone();

    // The harness already ran migrations (defer = false) → the matviews are built
    // and the current view-definition hash is stored.
    let real_hash = stored_hash(&pool)
        .await
        .expect("baseline view-definition hash must be present after harness setup");

    // Simulate a matview SQL *definition change* by poisoning the stored hash so
    // the gate sees a mismatch (current != stored).
    sqlx::query(
        "INSERT INTO pgmcp_metadata (key, value) VALUES ($1, 'stale-deadbeef')
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(HASH_KEY)
    .execute(&pool)
    .await
    .expect("poison the stored hash");

    // defer = true (daemon path): the matviews already EXIST, so `present()` is a
    // no-op and the hash-change rebuild is DEFERRED. A rebuild would have rewritten
    // the hash to the real value, so its staying poisoned proves no rebuild ran on
    // the critical path.
    pgmcp::db::migrations::run_migrations(&pool, &VectorConfig::default(), true)
        .await
        .expect("deferred migrations must succeed");
    assert_eq!(
        stored_hash(&pool).await.as_deref(),
        Some("stale-deadbeef"),
        "defer=true must NOT rebuild memory_unified on a definition (hash) change"
    );

    // defer = false (CLI inline / the daemon's background task): the hash mismatch
    // drives a rebuild that restores the real current view-definition hash.
    pgmcp::db::migrations::run_migrations(&pool, &VectorConfig::default(), false)
        .await
        .expect("inline migrations must succeed");
    assert_eq!(
        stored_hash(&pool).await,
        Some(real_hash),
        "defer=false must rebuild memory_unified and restore the current hash"
    );
}
