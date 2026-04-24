//! Mocked-DB correctness oracle for `suggest_merges`.
//!
//! Pins two algorithmic claims:
//!
//! 1. **Weighted Jaccard formula:** for two files with topic
//!    distributions Wa and Wb (per-topic membership totals), the
//!    overlap is `Σ min(wa, wb) / Σ max(wa, wb)`. Identical
//!    distributions → 1.0; disjoint → 0.0.
//! 2. **Union-find clustering across qualifying pairs:** if (A,B)
//!    and (B,C) both exceed `min_overlap`, then A, B, C land in
//!    the same merge group even though (A,C) was never directly
//!    evaluated.

use std::sync::Arc;

use arc_swap::ArcSwap;
use pgmcp::context::SystemContext;
use pgmcp::db::DbClient;
use pgmcp::db::queries::FileTopicDistributionRow;
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

fn dist(
    file_id: i64,
    rel: &str,
    topic_id: i32,
    label: &str,
    membership: f64,
) -> FileTopicDistributionRow {
    FileTopicDistributionRow {
        file_id,
        path: format!("/ws/p/{rel}"),
        relative_path: rel.into(),
        language: "markdown".into(),
        line_count: 30,
        size_bytes: 600,
        topic_id,
        topic_label: label.into(),
        keywords: None,
        total_membership: membership,
        chunks_in_topic: 1,
    }
}

#[tokio::test]
async fn suggest_merges_groups_two_high_overlap_files() {
    // File A and B both have membership {topic1: 5.0}. Identical
    // distributions ⇒ Jaccard = 1.0 ≥ 0.4 default threshold. File C
    // has only topic2 — disjoint from A and B.
    let mut mock = MockDbClient::new();
    mock.file_topic_distributions = vec![
        dist(1, "a.md", 1, "auth", 5.0),
        dist(2, "b.md", 1, "auth", 5.0),
        dist(3, "c.md", 2, "logging", 5.0),
    ];
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli(
            "suggest_merges",
            serde_json::json!({"project": "p", "language": "*"}),
        )
        .await
        .expect("call");
    let payload = text_of(&result);
    let v: serde_json::Value = serde_json::from_str(&payload).expect("json");
    let groups = v["merge_groups"].as_array().expect("merge_groups");
    assert_eq!(groups.len(), 1, "expected one merge group of 2 files");
    let files = groups[0]["files"].as_array().expect("files");
    let paths: std::collections::BTreeSet<&str> =
        files.iter().map(|f| f["path"].as_str().unwrap()).collect();
    assert_eq!(
        paths,
        std::collections::BTreeSet::from(["/ws/p/a.md", "/ws/p/b.md"])
    );
    let avg_overlap: f64 = groups[0]["avg_overlap"]
        .as_str()
        .unwrap()
        .parse()
        .expect("parse");
    assert!(
        (avg_overlap - 1.0).abs() < 1e-3,
        "identical distributions ⇒ overlap 1.0; got {avg_overlap}"
    );
}

#[tokio::test]
async fn suggest_merges_transitively_clusters_chain() {
    // A,B share 0.5, B,C share 0.5. A and C have only one topic each
    // and don't directly overlap above threshold — but union-find
    // should pull all three into one cluster via B.
    //
    // Distributions chosen so weighted Jaccard ≥ 0.4 for (A,B) and
    // (B,C):
    //   A = {1: 4.0}
    //   B = {1: 4.0, 2: 4.0}    (matches A on topic 1, C on topic 2)
    //   C = {2: 4.0}
    //   J(A,B) = min/max sum = (4 + 0)/(4 + 4) = 0.5 ≥ 0.4 ✓
    //   J(B,C) = (0 + 4)/(4 + 4) = 0.5 ≥ 0.4 ✓
    //   J(A,C) = (0 + 0)/(4 + 4) = 0.0 < 0.4 ✗
    let mut mock = MockDbClient::new();
    mock.file_topic_distributions = vec![
        dist(1, "a.md", 1, "auth", 4.0),
        dist(2, "b.md", 1, "auth", 4.0),
        dist(2, "b.md", 2, "logging", 4.0),
        dist(3, "c.md", 2, "logging", 4.0),
    ];
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli(
            "suggest_merges",
            serde_json::json!({"project": "p", "language": "*", "min_overlap": 0.4}),
        )
        .await
        .expect("call");
    let payload = text_of(&result);
    let v: serde_json::Value = serde_json::from_str(&payload).expect("json");
    let groups = v["merge_groups"].as_array().expect("merge_groups");
    assert_eq!(groups.len(), 1, "transitive cluster should be 1 group");
    let files = groups[0]["files"].as_array().expect("files");
    assert_eq!(files.len(), 3, "all three files joined transitively");
    let paths: std::collections::BTreeSet<&str> =
        files.iter().map(|f| f["path"].as_str().unwrap()).collect();
    assert_eq!(
        paths,
        std::collections::BTreeSet::from(["/ws/p/a.md", "/ws/p/b.md", "/ws/p/c.md"])
    );
}

#[tokio::test]
async fn suggest_merges_returns_no_groups_when_overlap_below_threshold() {
    // A has only topic 1; B has only topic 2 — disjoint, J=0.
    let mut mock = MockDbClient::new();
    mock.file_topic_distributions = vec![
        dist(1, "a.md", 1, "auth", 5.0),
        dist(2, "b.md", 2, "logging", 5.0),
    ];
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli(
            "suggest_merges",
            serde_json::json!({"project": "p", "language": "*", "min_overlap": 0.4}),
        )
        .await
        .expect("call");
    let payload = text_of(&result);
    let v: serde_json::Value = serde_json::from_str(&payload).expect("json");
    assert_eq!(v["merge_groups_found"], 0);
}
