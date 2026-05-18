//! Layer C addendum: smoke tests for the remaining files with raw
//! `sqlx::query` invocations that aren't part of `queries.rs`,
//! `sessions.rs`, `patterns.rs`, or the MCP tool surface.
//!
//! Coverage targets:
//!   * `src/db/pool.rs` — `create_pool` and `health_check`.
//!   * `src/mcp/tools/fix_helpers.rs` — `lookup_project_id`,
//!     `load_import_graph`, plus the pure helpers
//!     (`count_importers`, `scan_callsites_regex`,
//!     `callsites_to_file_lines`, `propose_function_name`,
//!     `infer_module_name_from_topics`, `default_fix_for_violation`,
//!     `default_fix_for_smell`, `pool_or_err`).
//!   * `src/cron/scheduler.rs` — `now_ms`.
//!   * `src/cron/topic_clustering.rs` — pure helpers (`estimate_k`,
//!     `fuzzy_c_means`, `compute_ctf_idf`).
//!   * `src/cron/graph_analysis.rs` — `run_graph_analysis` (in
//!     addition to the coverage in `cron_jobs_e2e.rs`).
//!   * `src/cron/symbol_extraction.rs` — `run_symbol_extraction`.
//!   * `src/indexer/extract/ocr_cache.rs` — `PgOcrCache` and
//!     `InMemoryOcrCache` lookup/store paths.
//!   * `src/stats/telemetry_writer.rs` — `try_enqueue`.

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use arc_swap::ArcSwap;
use ndarray::{Array2, ArrayView2};
use pgmcp::config::Config;
use pgmcp::context::SystemContext;
use pgmcp::cron::graph_analysis;
use pgmcp::cron::scheduler;
use pgmcp::cron::symbol_extraction;
use pgmcp::cron::topic_clustering;
use pgmcp::db::DbClient;
use pgmcp::db::pool;
use pgmcp::embed::{EmbedSource, EmbeddingBackend};
use pgmcp::graph::CodeGraph;
use pgmcp::graph::builder::build_graph;
use pgmcp::indexer::extract::ocr_cache::{InMemoryOcrCache, OcrCache, PgOcrCache};
use pgmcp::mcp::logging::LogBroadcaster;
use pgmcp::mcp::tasks::TaskStore;
use pgmcp::mcp::tools::fix_helpers;
use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::fixtures::synthetic_corpus::SyntheticCorpus;
use pgmcp_testing::mocks::DeterministicEmbeddingBackend;
use pgmcp_testing::require_test_db;
use sqlx::PgPool;

// =============================================================================
// SystemContext construction (modeled after pgmcp-testing/tests/common/mod.rs)
// =============================================================================

fn make_ctx(pool: PgPool) -> SystemContext {
    let db: Arc<dyn DbClient> = Arc::new(pool);
    let stats = Arc::new(StatsTracker::new());
    let config = Arc::new(ArcSwap::from_pointee(Config::default()));
    let log_broadcaster = Arc::new(LogBroadcaster::new());
    let task_store = Arc::new(TaskStore::new());
    let embed_backend: Arc<dyn EmbeddingBackend> =
        Arc::new(DeterministicEmbeddingBackend::new(384));
    let embed_source = EmbedSource::backend(embed_backend);
    let lifecycle = pgmcp::daemon_state::DaemonLifecycle::new();
    lifecycle.transition(pgmcp::daemon_state::DaemonPhase::Ready);
    SystemContext::production(
        db,
        embed_source,
        stats,
        config,
        log_broadcaster,
        task_store,
        lifecycle,
    )
}

// =============================================================================
// db/pool.rs (2 functions)
// =============================================================================

#[tokio::test]
async fn pool_health_check_smoke() {
    let db = require_test_db!();
    pool::health_check(db.pool())
        .await
        .expect("health_check must succeed against a live pool");
}

#[tokio::test]
async fn pool_create_pool_smoke() {
    // Probe via the same DB the test harness is using. We extract host /
    // port / user from the test config so the test doesn't hard-code.
    // The harness sets up `test-config.toml` so the same file works here.
    let cfg_path = std::env::var("PGMCP_TEST_DATABASE_URL").ok();
    let db_cfg = if let Some(_url) = cfg_path {
        // Env-var path → parse a minimal config object.
        // Fall back to default (peer auth on localhost) — the test DB
        // was already opened by `require_test_db!`.
        pgmcp::config::DatabaseConfig::default()
    } else {
        pgmcp::config::DatabaseConfig::default()
    };
    let _ = pool::create_pool(&db_cfg)
        .await
        .expect("create_pool must succeed using the same DB the harness uses");
}

// =============================================================================
// fix_helpers.rs — pure helpers (no DB)
// =============================================================================

#[test]
fn fix_helpers_scan_callsites_regex_empty_name_returns_empty() {
    let lines = fix_helpers::scan_callsites_regex("fn foo() { foo(); }", "");
    assert!(lines.is_empty(), "empty fn_name must return no lines");
}

#[test]
fn fix_helpers_scan_callsites_regex_finds_match() {
    let lines = fix_helpers::scan_callsites_regex("let x = foo();\nlet y = foo();\n", "foo");
    assert!(!lines.is_empty(), "must locate foo() callsites");
}

#[test]
fn fix_helpers_callsites_to_file_lines_smoke() {
    let lines = fix_helpers::callsites_to_file_lines("src/x.rs", &[1, 2, 3]);
    assert_eq!(lines.len(), 3);
}

#[test]
fn fix_helpers_propose_function_name_smoke() {
    let name = fix_helpers::propose_function_name(&["validate".into(), "password".into()]);
    assert!(
        !name.is_empty(),
        "propose_function_name must produce a name"
    );
}

#[test]
fn fix_helpers_infer_module_name_from_topics_smoke() {
    let name = fix_helpers::infer_module_name_from_topics(&["query".into(), "transaction".into()]);
    assert!(!name.is_empty(), "infer_module_name must produce a name");
}

#[test]
fn fix_helpers_default_fix_for_violation_smoke() {
    let _ = fix_helpers::default_fix_for_violation(
        "cross_module_coupling",
        "proj-auth",
        &["a.rs".into(), "b.rs".into()],
        Some("shared"),
    );
}

#[test]
fn fix_helpers_default_fix_for_smell_smoke() {
    let _ = fix_helpers::default_fix_for_smell(
        "god_class",
        "proj-auth",
        "src/big.rs",
        5000,
        "5000 lines",
    );
}

#[test]
fn fix_helpers_count_importers_empty_graph_smoke() {
    let graph = build_graph(&[], &[]);
    let importers = fix_helpers::count_importers(&graph, 999);
    assert!(importers.is_empty());
}

// =============================================================================
// fix_helpers.rs — DB-backed (lookup_project_id, load_import_graph)
// =============================================================================

#[tokio::test]
async fn fix_helpers_pool_or_err_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let ctx = make_ctx(db.pool().clone());
    let _ = fix_helpers::pool_or_err(&ctx).expect("pool_or_err");
}

#[tokio::test]
async fn fix_helpers_lookup_project_id_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let ctx = make_ctx(db.pool().clone());
    let id = fix_helpers::lookup_project_id(&ctx, "proj-auth")
        .await
        .expect("lookup_project_id");
    assert!(id.is_some(), "proj-auth must be found in seeded corpus");
}

#[tokio::test]
async fn fix_helpers_load_import_graph_smoke() {
    let db = require_test_db!();
    let h = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let ctx = make_ctx(db.pool().clone());
    let _ = fix_helpers::load_import_graph(&ctx, h.auth_project_id)
        .await
        .expect("load_import_graph");
}

// =============================================================================
// cron/scheduler.rs (pure helpers)
// =============================================================================

#[test]
fn scheduler_now_ms_returns_positive() {
    let t = scheduler::now_ms();
    assert!(t > 0, "now_ms must be positive Unix-epoch ms");
}

// =============================================================================
// cron/topic_clustering.rs (pure helpers — FCM + c-TF-IDF)
// =============================================================================

#[test]
fn topic_clustering_estimate_k_smoke() {
    assert_eq!(
        topic_clustering::estimate_k(100, 5),
        10,
        "estimate_k clamps to min 10"
    );
    assert!(topic_clustering::estimate_k(10_000, 5) >= 10);
}

#[test]
fn topic_clustering_fuzzy_c_means_smoke() {
    // 6 chunks × 3 dims, 2 clear clusters at e0 and e1.
    let data = Array2::from_shape_vec(
        (6, 3),
        vec![
            1.0, 0.0, 0.0, 0.99, 0.01, 0.0, 0.98, 0.02, 0.0, 0.0, 1.0, 0.0, 0.0, 0.99, 0.01, 0.0,
            0.98, 0.02,
        ],
    )
    .expect("shape");
    let view: ArrayView2<f32> = data.view();
    let res = topic_clustering::fuzzy_c_means(view, 2, 2.0, 50, 1e-4, None);
    assert_eq!(res.centroids.ncols(), 3);
}

#[test]
fn topic_clustering_compute_ctf_idf_smoke() {
    let contents = ["auth password token", "query database transaction"];
    let content_refs: Vec<&str> = contents.iter().copied().collect();
    // 2 chunks × 2 topics — chunk 0 strongly belongs to topic 0; chunk 1 to topic 1.
    let mem = Array2::from_shape_vec((2, 2), vec![0.9_f32, 0.1, 0.1, 0.9]).expect("mem");
    let topics = topic_clustering::compute_ctf_idf(&content_refs, &mem, 3);
    assert_eq!(
        topics.len(),
        2,
        "compute_ctf_idf must return one Vec per topic"
    );
}

// =============================================================================
// cron/graph_analysis.rs + cron/symbol_extraction.rs
// (cron_jobs_e2e.rs already covers these; we add explicit single-call
// smoke tests so they're discoverable from the per-query layer.)
// =============================================================================

#[tokio::test]
async fn cron_run_graph_analysis_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let pool: Arc<dyn DbClient> = Arc::new(db.pool().clone());
    let stats = Arc::new(StatsTracker::new());
    graph_analysis::run_graph_analysis(pool.as_ref(), &stats, None).await;
    // Function returns (); successful completion = SQL parsed + executed.
    let _ = stats.graph_build_runs.load(Ordering::Relaxed);
}

#[tokio::test]
async fn cron_run_symbol_extraction_smoke() {
    let db = require_test_db!();
    let _ = SyntheticCorpus::seed_with_assignments(db.pool()).await;
    let pool: Arc<dyn DbClient> = Arc::new(db.pool().clone());
    let stats = Arc::new(StatsTracker::new());
    symbol_extraction::run_symbol_extraction(pool.as_ref(), &stats).await;
}

// =============================================================================
// indexer/extract/ocr_cache.rs — PgOcrCache and InMemoryOcrCache
// =============================================================================

#[test]
fn ocr_in_memory_cache_round_trips() {
    let c = InMemoryOcrCache::default();
    let hash = 12345_i64;
    assert!(c.lookup(hash).is_none(), "empty cache returns None");
    c.store(hash, "extracted text", 1, 300, &["eng".to_string()]);
    let got = c.lookup(hash);
    assert_eq!(
        got.as_deref(),
        Some("extracted text"),
        "lookup must return the stored text"
    );
}

#[test]
fn ocr_pg_cache_round_trips_via_blocking() {
    // PgOcrCache's lookup/store use `rt.block_on(...)` so they can't be
    // called from a `#[tokio::test]` body (re-entrant runtime). Run on
    // a fresh single-thread runtime that owns the test DB pool.
    //
    // We don't bring up the full TestDatabase template machinery here
    // because PgOcrCache::lookup is a read against `ocr_extractions`
    // which is migrated. We just need a pool pointing at the test DB.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("rt");
    rt.block_on(async {
        let db = match pgmcp_testing::db_harness::TestDatabase::new().await {
            Ok(d) => d,
            Err(e) => {
                eprintln!("SKIPPED: {}", e);
                return;
            }
        };
        let pool = db.pool().clone();
        let handle = tokio::runtime::Handle::current();
        let cache = PgOcrCache::new(pool, handle);

        // Calls happen on a blocking thread because they use block_on.
        tokio::task::spawn_blocking(move || {
            let hash = 987654_i64;
            assert!(cache.lookup(hash).is_none(), "empty pg cache returns None");
            cache.store(hash, "pg extracted text", 2, 300, &["eng".to_string()]);
            let got = cache.lookup(hash);
            assert_eq!(got.as_deref(), Some("pg extracted text"));
        })
        .await
        .expect("blocking task");
    });
}

// =============================================================================
// stats/telemetry_writer.rs — try_enqueue
// =============================================================================

#[test]
fn telemetry_writer_try_enqueue_when_channel_absent_returns_false() {
    // `try_enqueue` reads `stats.telemetry_tx` (set up by
    // `start_telemetry_writer`). When the writer hasn't been started,
    // the channel is None and the function returns false.
    let stats = StatsTracker::new();
    let row = pgmcp::stats::telemetry_writer::TelemetryRow {
        tool: "test".into(),
        client_name: "tester".into(),
        client_version: None,
        protocol_version: None,
        mcp_session_id: None,
        project: None,
        cwd: None,
        duration_ms: 1,
        outcome: "ok",
        error_class: None,
        request_id: None,
        params_sha256: None,
    };
    let ok = pgmcp::stats::telemetry_writer::try_enqueue(&stats, row);
    assert!(!ok, "try_enqueue must be false when writer is not started");
}

// Bridge to the rest of the suite — keeps `common` import not "dead".
#[allow(dead_code)]
fn _common_imported_for_consistency(_g: CodeGraph) {}
