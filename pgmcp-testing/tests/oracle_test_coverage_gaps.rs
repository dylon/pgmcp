//! Mocked-DB correctness oracle for `test_coverage_gaps`.
//!
//! Pins the per-topic test/impl classification:
//!
//!   test_ratio = test_chunks / (test_chunks + impl_chunks)
//!   status = well-tested  if test_ratio > 0.30
//!          | under-tested if 0.01 < test_ratio ≤ 0.30
//!          | untested     if test_ratio ≤ 0.01
//!
//! Mirrors `oracle_doc_coverage_gaps.rs` (same shape, different
//! threshold band).

use std::sync::Arc;

use arc_swap::ArcSwap;
use pgmcp::context::SystemContext;
use pgmcp::db::DbClient;
use pgmcp::db::queries::TopicCoverageRow;
use pgmcp::embed::EmbedSource;
use pgmcp::mcp::logging::LogBroadcaster;
use pgmcp::mcp::server::McpServer;
use pgmcp::mcp::tasks::TaskStore;
use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::fixtures::test_config;
use pgmcp_testing::mocks::{DeterministicEmbeddingBackend, MockDbClient};

fn server_with_mock(mock: MockDbClient) -> McpServer {
    let db: Arc<dyn DbClient> = Arc::new(mock);
    let stats = Arc::new(StatsTracker::new());
    let config = Arc::new(ArcSwap::from_pointee(test_config()));
    let log_broadcaster = Arc::new(LogBroadcaster::new());
    let task_store = Arc::new(TaskStore::new());
    let embed_backend: Arc<dyn pgmcp::embed::EmbeddingBackend> =
        Arc::new(DeterministicEmbeddingBackend::new(384));
    let embed_source = EmbedSource::backend(embed_backend);
    let ctx = SystemContext::production(
        db,
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
    McpServer::new(ctx)
}

fn text_of(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .next()
        .expect("text content present")
}

#[tokio::test]
async fn test_coverage_gaps_classifies_each_topic_per_threshold_table() {
    let mut mock = MockDbClient::new();
    mock.test_topic_coverage = vec![
        // well-tested: test=10, impl=5 → ratio 0.667
        TopicCoverageRow {
            topic_id: 1,
            label: "well".into(),
            test_chunks: 10,
            impl_chunks: 5,
        },
        // under-tested: test=2, impl=18 → ratio 0.10
        TopicCoverageRow {
            topic_id: 2,
            label: "under".into(),
            test_chunks: 2,
            impl_chunks: 18,
        },
        // untested: test=0, impl=20 → ratio 0
        TopicCoverageRow {
            topic_id: 3,
            label: "none".into(),
            test_chunks: 0,
            impl_chunks: 20,
        },
    ];
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli("test_coverage_gaps", serde_json::json!({"project": "p"}))
        .await
        .expect("call");
    let payload = text_of(&result);
    let v: serde_json::Value = serde_json::from_str(&payload).expect("json");
    let topics = v["topics"].as_array().expect("topics");
    assert_eq!(topics.len(), 3);
    let by_label: std::collections::HashMap<&str, &serde_json::Value> = topics
        .iter()
        .map(|t| (t["label"].as_str().unwrap(), t))
        .collect();
    assert_eq!(by_label["well"]["status"], "well-tested");
    assert_eq!(by_label["under"]["status"], "under-tested");
    assert_eq!(by_label["none"]["status"], "untested");

    assert_eq!(v["total_test_chunks"].as_i64(), Some(12));
    assert_eq!(v["total_impl_chunks"].as_i64(), Some(43));
}

#[tokio::test]
async fn test_coverage_gaps_sorts_topics_lowest_ratio_first() {
    let mut mock = MockDbClient::new();
    mock.test_topic_coverage = vec![
        TopicCoverageRow {
            topic_id: 1,
            label: "best".into(),
            test_chunks: 9,
            impl_chunks: 1,
        },
        TopicCoverageRow {
            topic_id: 2,
            label: "middle".into(),
            test_chunks: 5,
            impl_chunks: 5,
        },
        TopicCoverageRow {
            topic_id: 3,
            label: "worst".into(),
            test_chunks: 0,
            impl_chunks: 10,
        },
    ];
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli("test_coverage_gaps", serde_json::json!({"project": "p"}))
        .await
        .expect("call");
    let payload = text_of(&result);
    let v: serde_json::Value = serde_json::from_str(&payload).expect("json");
    let topics = v["topics"].as_array().expect("topics");
    let order: Vec<&str> = topics
        .iter()
        .map(|t| t["label"].as_str().unwrap())
        .collect();
    assert_eq!(order, vec!["worst", "middle", "best"]);
}
