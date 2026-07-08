//! Regression: a MISSING `memory_unified_edges` matview must SELF-HEAL on the
//! next migration pass — even when the stored views-hash still MATCHES — and
//! WITHOUT rebuilding `memory_unified_nodes`, `memory_unified_node_vectors`, or
//! the (multi-minute) HNSW index that now lives on the vectors matview (the
//! 2026-07-06 nodes/vectors split).
//!
//! The 2026-06-20 `graph_neighbors` outage: `build_memory_unified_views` drops
//! `memory_unified_edges` FIRST and (pre-fix) only recreated it AFTER the nodes
//! HNSW build, so a SIGKILL/restart in that window left edges dropped. The stored
//! views-hash (written only on a *full* successful build) still matched, so the
//! hash gate skipped and the startup guard (which only checked nodes) returned
//! Ok — the edges matview stayed missing across restarts and every unified-graph
//! tool (`graph_neighbors`, `memory_neighbors`, `memory_path_search`,
//! `memory_ppr_search`) errored with `relation "memory_unified_edges" does not
//! exist`.
//!
//! The fix (`matviews_present` + `ensure_edges_only`): the startup guard and the
//! hash gate BOTH check BOTH matviews and repair a missing edges matview cheaply
//! (no HNSW). This test pins that via the public `run_migrations` API, exercising
//! BOTH gates — `defer=true` (startup critical-path guard) and `defer=false`
//! (hash gate) — and proves the nodes matview + HNSW index are NOT rebuilt
//! (`relfilenode` unchanged) by the edges-only repair.

use pgmcp::config::VectorConfig;
use pgmcp_testing::require_test_db;

const HASH_KEY: &str = "memory_unified_views_def_hash";

async fn edges_present(pool: &sqlx::PgPool) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pg_class WHERE relname = 'memory_unified_edges' AND relkind = 'm')",
    )
    .fetch_one(pool)
    .await
    .expect("probe memory_unified_edges presence")
}

async fn edge_index_count(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_class
          WHERE relkind = 'i'
            AND relname IN ('idx_memory_unified_edges_uq', 'idx_memory_unified_edges_from',
                            'idx_memory_unified_edges_to', 'idx_memory_unified_edges_valid')",
    )
    .fetch_one(pool)
    .await
    .expect("count edge indexes")
}

/// A matview/index's physical file id. DROP+CREATE assigns a new `relfilenode`,
/// so an unchanged value proves the relation was NOT rebuilt.
async fn relfilenode(pool: &sqlx::PgPool, relname: &str) -> Option<i64> {
    sqlx::query_scalar("SELECT relfilenode::bigint FROM pg_class WHERE relname = $1")
        .bind(relname)
        .fetch_optional(pool)
        .await
        .expect("read relfilenode")
}

async fn stored_hash(pool: &sqlx::PgPool) -> Option<String> {
    sqlx::query_scalar("SELECT value FROM pgmcp_metadata WHERE key = $1")
        .bind(HASH_KEY)
        .fetch_optional(pool)
        .await
        .expect("read view-definition hash")
}

async fn drop_edges(pool: &sqlx::PgPool) {
    sqlx::query("DROP MATERIALIZED VIEW IF EXISTS memory_unified_edges")
        .execute(pool)
        .await
        .expect("drop memory_unified_edges");
}

#[tokio::test]
async fn missing_edges_matview_self_heals_without_rebuilding_nodes() {
    let db = require_test_db!();
    let pool = db.pool().clone();

    // Harness already ran migrations (defer=false) → both matviews exist + hash stored.
    assert!(
        edges_present(&pool).await,
        "harness setup must build the edges matview"
    );
    let baseline_hash = stored_hash(&pool)
        .await
        .expect("baseline views-hash present after harness setup");
    let nodes_rfn = relfilenode(&pool, "memory_unified_nodes")
        .await
        .expect("nodes matview present after harness setup");
    // Post-2026-07-06 split: the embedding + HNSW live in the separate
    // `memory_unified_node_vectors` matview. The edges-only repair must rebuild
    // NONE of nodes / vectors / the (expensive) vectors HNSW.
    let vectors_rfn = relfilenode(&pool, "memory_unified_node_vectors")
        .await
        .expect("vectors matview present after harness setup");
    let hnsw_rfn = relfilenode(&pool, "idx_memory_unified_node_vectors_embedding")
        .await
        .expect("vectors HNSW index present after harness setup");

    // ---- Gate A — the daemon startup critical-path guard (defer = true). ----
    // Reproduce the live failure EXACTLY: edges dropped, stored hash left MATCHING.
    drop_edges(&pool).await;
    assert!(!edges_present(&pool).await, "edges dropped for Gate A");
    assert_eq!(
        stored_hash(&pool).await.as_deref(),
        Some(baseline_hash.as_str()),
        "the stored hash must still match — that is the masking precondition"
    );

    pgmcp::db::migrations::run_migrations(&pool, &VectorConfig::default(), true)
        .await
        .expect("deferred migrations must succeed");

    assert!(
        edges_present(&pool).await,
        "startup guard (defer=true) must self-heal the missing edges matview"
    );
    assert_eq!(
        edge_index_count(&pool).await,
        4,
        "all 4 edge indexes rebuilt (Gate A)"
    );
    assert_eq!(
        relfilenode(&pool, "memory_unified_nodes").await,
        Some(nodes_rfn),
        "nodes matview must NOT be rebuilt by the edges-only repair (Gate A)"
    );
    assert_eq!(
        relfilenode(&pool, "memory_unified_node_vectors").await,
        Some(vectors_rfn),
        "vectors matview must NOT be rebuilt by the edges-only repair (Gate A)"
    );
    assert_eq!(
        relfilenode(&pool, "idx_memory_unified_node_vectors_embedding").await,
        Some(hnsw_rfn),
        "vectors HNSW index must NOT be rebuilt by the edges-only repair (Gate A)"
    );

    // ---- Gate B — the hash gate (defer = false): existence dominates. ----
    // This is the assertion that FAILS on the pre-fix code: a matching hash made
    // `ensure_memory_unified_views` return Ok without checking that edges exists.
    drop_edges(&pool).await;
    assert!(!edges_present(&pool).await, "edges dropped for Gate B");

    pgmcp::db::migrations::run_migrations(&pool, &VectorConfig::default(), false)
        .await
        .expect("inline migrations must succeed");

    assert!(
        edges_present(&pool).await,
        "hash gate (defer=false) must repair missing edges despite a MATCHING hash"
    );
    assert_eq!(
        edge_index_count(&pool).await,
        4,
        "all 4 edge indexes rebuilt (Gate B)"
    );
    assert_eq!(
        relfilenode(&pool, "memory_unified_nodes").await,
        Some(nodes_rfn),
        "nodes matview must NOT be rebuilt by the hash-gate edges-only repair (Gate B)"
    );
    assert_eq!(
        relfilenode(&pool, "memory_unified_node_vectors").await,
        Some(vectors_rfn),
        "vectors matview must NOT be rebuilt by the hash-gate edges-only repair (Gate B)"
    );
    assert_eq!(
        relfilenode(&pool, "idx_memory_unified_node_vectors_embedding").await,
        Some(hnsw_rfn),
        "vectors HNSW index must NOT be rebuilt by the hash-gate edges-only repair (Gate B)"
    );

    // End-to-end: a query over the repaired matview (what `graph_neighbors` walks)
    // executes instead of erroring with "relation does not exist".
    let _: i64 = sqlx::query_scalar("SELECT count(*) FROM memory_unified_edges")
        .fetch_one(&pool)
        .await
        .expect("repaired memory_unified_edges must be queryable");
}
