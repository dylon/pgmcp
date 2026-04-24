//! Mocked-DB correctness oracle for `doc_coverage_gaps`.
//!
//! Pins the per-topic doc/code classification:
//!
//!   doc_ratio = doc_chunks / (doc_chunks + code_chunks)
//!   status = well-documented  if doc_ratio > 0.30
//!          | under-documented if 0.05 < doc_ratio ≤ 0.30
//!          | undocumented     if doc_ratio ≤ 0.05
//!
//! Also asserts the worst-first sort and the doc/code totals roll-up.

use std::sync::Arc;

use arc_swap::ArcSwap;
use pgmcp::context::SystemContext;
use pgmcp::db::DbClient;
use pgmcp::db::queries::DocCoverageRow;
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
    let ctx =
        SystemContext::production(db, embed_source, stats, config, log_broadcaster, task_store);
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
async fn doc_coverage_gaps_classifies_each_topic_per_threshold_table() {
    // Three topics with distinct doc-ratio bands:
    //   well:  doc=10, code=5  → ratio = 10/15 ≈ 0.667 > 0.30
    //   under: doc=2,  code=18 → ratio = 0.10 in (0.05, 0.30]
    //   undoc: doc=0,  code=20 → ratio = 0.0 ≤ 0.05
    let mut mock = MockDbClient::new();
    mock.doc_topic_coverage = vec![
        DocCoverageRow {
            topic_id: 1,
            label: "well_doc".into(),
            keywords: None,
            doc_chunks: 10,
            code_chunks: 5,
        },
        DocCoverageRow {
            topic_id: 2,
            label: "under_doc".into(),
            keywords: None,
            doc_chunks: 2,
            code_chunks: 18,
        },
        DocCoverageRow {
            topic_id: 3,
            label: "no_doc".into(),
            keywords: None,
            doc_chunks: 0,
            code_chunks: 20,
        },
    ];
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli("doc_coverage_gaps", serde_json::json!({"project": "p"}))
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
    assert_eq!(by_label["well_doc"]["status"], "well-documented");
    assert_eq!(by_label["under_doc"]["status"], "under-documented");
    assert_eq!(by_label["no_doc"]["status"], "undocumented");

    // Totals roll-up.
    assert_eq!(v["total_doc_chunks"].as_i64(), Some(12));
    assert_eq!(v["total_code_chunks"].as_i64(), Some(43));
}

#[tokio::test]
async fn doc_coverage_gaps_sorts_topics_worst_ratio_first() {
    // Three topics with strictly different ratios. Worst (lowest) first.
    let mut mock = MockDbClient::new();
    mock.doc_topic_coverage = vec![
        DocCoverageRow {
            topic_id: 1,
            label: "best".into(),
            keywords: None,
            doc_chunks: 9,
            code_chunks: 1,
        },
        DocCoverageRow {
            topic_id: 2,
            label: "middle".into(),
            keywords: None,
            doc_chunks: 5,
            code_chunks: 5,
        },
        DocCoverageRow {
            topic_id: 3,
            label: "worst".into(),
            keywords: None,
            doc_chunks: 1,
            code_chunks: 9,
        },
    ];
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli("doc_coverage_gaps", serde_json::json!({"project": "p"}))
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

#[tokio::test]
async fn doc_coverage_gaps_emits_guidance_when_no_topics() {
    let mock = MockDbClient::new();
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli("doc_coverage_gaps", serde_json::json!({"project": "p"}))
        .await
        .expect("call");
    let payload = text_of(&result);
    assert!(
        payload.contains("Run discover_topics first"),
        "expected discover_topics-first guidance; got:\n{payload}"
    );
}
