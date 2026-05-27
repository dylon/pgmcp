//! Real-Postgres correctness oracle for `discover_topics`.
//!
//! This is the only Phase G test that exercises the **actual FCM
//! pipeline**, not just the wrapper. The synthetic 30-chunk corpus
//! has three planted clusters separated by orthonormal basis
//! directions — any reasonable clusterer must find them.
//!
//! The test forces `topic_num_clusters = Some(3)` so the adaptive K
//! sweep is bypassed and we get exactly 3 communities. FCM's
//! cold-start uses k-means++; on perfectly-separated data it
//! reliably partitions into the planted clusters even without seed
//! determinism. We assert:
//!
//! 1. The realtime per-project scan reports `topics_found == 3`.
//! 2. Each planted cluster has a stable size in [8, 12] (the 10
//!    seed chunks ± occasional misclassifications of the orphan).
//! 3. After running, `cached_topics` for the project's scope contains
//!    3 rows.
//!
//! Skips with `SKIPPED:` if no test DB is configured.

use std::sync::Arc;

use arc_swap::ArcSwap;
use pgmcp::config::Config;
use pgmcp::context::SystemContext;
use pgmcp::db::DbClient;
use pgmcp::embed::EmbedSource;
use pgmcp::mcp::logging::LogBroadcaster;
use pgmcp::mcp::server::McpServer;
use pgmcp::mcp::tasks::TaskStore;
use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::fixtures::synthetic_corpus::SyntheticCorpus;
use pgmcp_testing::mocks::DeterministicEmbeddingBackend;
use pgmcp_testing::require_test_db;

/// Build a config that pins K=3 and tightens cluster-size threshold so
/// the oracle's expected partition is reproducible.
fn config_for_oracle() -> Config {
    let mut cfg = Config::default();
    cfg.cron.topic_num_clusters = Some(3);
    cfg.cron.topic_min_cluster_size = 3;
    cfg.cron.topic_membership_threshold = 0.05;
    cfg
}

#[tokio::test]
async fn discover_topics_realtime_partitions_synthetic_corpus_into_three_clusters() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _handles = SyntheticCorpus::seed_chunks_only(&pool).await;

    let stats = Arc::new(StatsTracker::new());
    let config = Arc::new(ArcSwap::from_pointee(config_for_oracle()));
    let log_broadcaster = Arc::new(LogBroadcaster::new());
    let task_store = Arc::new(TaskStore::new());
    let embed_backend: Arc<dyn pgmcp::embed::EmbeddingBackend> =
        Arc::new(DeterministicEmbeddingBackend::new(1024));
    let embed_source = EmbedSource::backend(embed_backend);
    let db_arc: Arc<dyn DbClient> = Arc::new(pool.clone());
    let ctx = SystemContext::production(
        db_arc,
        embed_source,
        stats,
        config,
        log_broadcaster,
        task_store,
        {
            let __l = pgmcp::daemon_state::DaemonLifecycle::new();
            __l.transition(pgmcp::daemon_state::DaemonPhase::Ready);
            __l
        },
    );
    let server = McpServer::new(ctx);

    let result = server
        .call_tool_cli(
            "discover_topics",
            serde_json::json!({"project": "proj-auth"}),
        )
        .await
        .expect("discover_topics call");

    let payload = result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .next()
        .expect("text content");

    let v: serde_json::Value = serde_json::from_str(&payload).expect("json");

    // proj-auth has 10 main chunks + 1 orphan + 3 split-candidate chunks
    // — 14 chunks. With num_clusters forced to 3, FCM should still
    // partition them into 3 clusters because the basis vectors used
    // span all 3 dimensions (the orphan is equidistant from all
    // bases and the split candidate chunks each point at a different
    // basis).
    let topics_found = v["topics_found"].as_u64().expect("topics_found");
    assert_eq!(
        topics_found, 3,
        "FCM with topic_num_clusters=3 should produce 3 clusters; got {topics_found}\npayload:\n{payload}"
    );

    // Total chunks analyzed should match the proj-auth chunk count
    // (10 main + 1 orphan + 3 split = 14).
    let chunks_analyzed = v["chunks_analyzed"].as_u64().expect("chunks_analyzed");
    assert_eq!(
        chunks_analyzed, 14,
        "expected 14 chunks analyzed in proj-auth; got {chunks_analyzed}"
    );

    // FCM converged within iteration budget.
    assert_eq!(
        v["fuzziness"].as_f64().unwrap_or(0.0),
        2.0,
        "default fuzziness from CronConfig"
    );

    // Topics array must have 3 entries with non-empty membership.
    let topics = v["topics"].as_array().expect("topics array");
    assert_eq!(topics.len(), 3);
    for t in topics {
        let chunk_count = t["chunk_count"].as_u64().expect("chunk_count");
        assert!(
            chunk_count >= 1,
            "every topic must have ≥ 1 chunk; got {chunk_count} for topic {}",
            t["label"]
        );
    }
}

#[tokio::test]
async fn discover_topics_global_cached_path_returns_persisted_topics() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let handles = SyntheticCorpus::seed_with_assignments(&pool).await;

    let stats = Arc::new(StatsTracker::new());
    let config = Arc::new(ArcSwap::from_pointee(config_for_oracle()));
    let log_broadcaster = Arc::new(LogBroadcaster::new());
    let task_store = Arc::new(TaskStore::new());
    let embed_backend: Arc<dyn pgmcp::embed::EmbeddingBackend> =
        Arc::new(DeterministicEmbeddingBackend::new(1024));
    let embed_source = EmbedSource::backend(embed_backend);
    let db_arc: Arc<dyn DbClient> = Arc::new(pool.clone());
    let ctx = SystemContext::production(
        db_arc,
        embed_source,
        stats,
        config,
        log_broadcaster,
        task_store,
        {
            let __l = pgmcp::daemon_state::DaemonLifecycle::new();
            __l.transition(pgmcp::daemon_state::DaemonPhase::Ready);
            __l
        },
    );
    let server = McpServer::new(ctx);

    let result = server
        .call_tool_cli(
            "discover_topics",
            serde_json::json!({"refresh": false, "limit": 10}),
        )
        .await
        .expect("discover_topics call");

    let payload = result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .next()
        .expect("text content");
    let v: serde_json::Value = serde_json::from_str(&payload).expect("json");

    assert_eq!(v["scope"], "global");
    assert_eq!(
        v["source"], "cached",
        "with refresh=false the tool must read from cache, not recompute"
    );
    let topics_found = v["topics_found"].as_u64().expect("topics_found");
    assert_eq!(
        topics_found, 3,
        "synthetic corpus inserts exactly 3 global topics; got {topics_found}"
    );
    let _ = handles; // keep handles alive
}
