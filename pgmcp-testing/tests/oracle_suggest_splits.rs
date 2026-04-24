//! Mocked-DB correctness oracle for `suggest_splits`.
//!
//! Pins three claims:
//!
//! 1. **Shannon entropy formula:** for a file whose chunk
//!    membership distribution across topics is uniform over K
//!    topics, entropy = log2(K). Verifies the formula and the
//!    `min_entropy` filter.
//! 2. **min_topics filter:** a file with fewer distinct topics
//!    than `min_topics` is suppressed even if entropy is high.
//! 3. **Topic transition detection:** when consecutive chunks
//!    have different dominant topics, each transition is reported
//!    with the `topic_before`/`topic_after` labels.

use std::sync::Arc;

use arc_swap::ArcSwap;
use pgmcp::context::SystemContext;
use pgmcp::db::DbClient;
use pgmcp::db::queries::ChunkTopicDetailRow;
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

fn detail(
    file_id: i64,
    rel: &str,
    chunk_id: i64,
    chunk_index: i32,
    start_line: i32,
    content: &str,
    topic_id: i32,
    topic_label: &str,
    membership: f64,
) -> ChunkTopicDetailRow {
    ChunkTopicDetailRow {
        file_id,
        path: format!("/ws/p/{rel}"),
        relative_path: rel.into(),
        language: "markdown".into(),
        line_count: 60,
        size_bytes: 1200,
        chunk_id,
        chunk_index,
        start_line,
        end_line: start_line + 9,
        chunk_content: content.into(),
        topic_id,
        topic_label: topic_label.into(),
        membership_score: membership,
    }
}

#[tokio::test]
async fn suggest_splits_flags_high_entropy_file_with_log2_3_value() {
    // 3 chunks each with full membership in a different topic.
    // Total per-topic membership in the file: each = 1.0; sum = 3.0
    // → proportions p_i = 1/3 each → entropy = log2(3) ≈ 1.585.
    // 1.585 ≥ 1.5 (default min_entropy) and 3 ≥ 3 (default min_topics).
    let mut mock = MockDbClient::new();
    mock.chunk_topic_details = vec![
        detail(
            1,
            "mixed.md",
            10,
            0,
            1,
            "## Auth\nvalidate password",
            1,
            "auth",
            1.0,
        ),
        detail(
            1,
            "mixed.md",
            11,
            1,
            11,
            "## Database\nrun query",
            2,
            "database",
            1.0,
        ),
        detail(
            1,
            "mixed.md",
            12,
            2,
            21,
            "## Logging\nemit log",
            3,
            "logging",
            1.0,
        ),
    ];
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli(
            "suggest_splits",
            serde_json::json!({"project": "p", "language": "*"}),
        )
        .await
        .expect("call");
    let payload = text_of(&result);
    let v: serde_json::Value = serde_json::from_str(&payload).expect("json");
    let candidates = v["candidates"].as_array().expect("candidates");
    assert_eq!(candidates.len(), 1, "expected 1 split candidate");
    let entry = &candidates[0];
    assert_eq!(entry["path"], "/ws/p/mixed.md");
    assert_eq!(entry["topic_count"], 3);
    let entropy: f64 = entry["entropy"].as_str().unwrap().parse().expect("parse");
    let expected = (3.0_f64).log2();
    assert!(
        (entropy - expected).abs() < 1e-2,
        "entropy = {entropy}, expected log2(3) ≈ {expected}"
    );
    // Two transitions: chunk0→chunk1 (auth→database), chunk1→chunk2
    // (database→logging).
    assert_eq!(entry["topic_transitions"], 2);
}

#[tokio::test]
async fn suggest_splits_suppresses_files_with_too_few_topics() {
    // File with chunks in only 2 distinct topics — below default min_topics=3.
    let mut mock = MockDbClient::new();
    mock.chunk_topic_details = vec![
        detail(1, "low.md", 10, 0, 1, "## A", 1, "auth", 1.0),
        detail(1, "low.md", 11, 1, 11, "## B", 2, "database", 1.0),
    ];
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli(
            "suggest_splits",
            serde_json::json!({"project": "p", "language": "*"}),
        )
        .await
        .expect("call");
    let payload = text_of(&result);
    let v: serde_json::Value = serde_json::from_str(&payload).expect("json");
    assert_eq!(
        v["split_candidates_found"], 0,
        "files with < min_topics should be filtered out"
    );
}

#[tokio::test]
async fn suggest_splits_records_transition_topics_in_order() {
    // 4 chunks alternating topics A, B, A, B. Three transitions.
    // Distribution: A=2, B=2, total=4 → proportions 0.5 each
    // → entropy = 1.0 < 1.5 default. Lower min_entropy to capture.
    // Also need min_topics=2.
    let mut mock = MockDbClient::new();
    mock.chunk_topic_details = vec![
        detail(1, "alt.md", 10, 0, 1, "## A1", 1, "auth", 1.0),
        detail(1, "alt.md", 11, 1, 11, "## B1", 2, "database", 1.0),
        detail(1, "alt.md", 12, 2, 21, "## A2", 1, "auth", 1.0),
        detail(1, "alt.md", 13, 3, 31, "## B2", 2, "database", 1.0),
    ];
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli(
            "suggest_splits",
            serde_json::json!({
                "project": "p",
                "language": "*",
                "min_entropy": 0.9,
                "min_topics": 2,
            }),
        )
        .await
        .expect("call");
    let payload = text_of(&result);
    let v: serde_json::Value = serde_json::from_str(&payload).expect("json");
    let candidates = v["candidates"].as_array().expect("candidates");
    assert_eq!(candidates.len(), 1);
    let entry = &candidates[0];
    assert_eq!(
        entry["topic_transitions"], 3,
        "ABAB has 3 transitions: A→B, B→A, A→B"
    );
    let splits = entry["suggested_splits"].as_array().expect("splits");
    assert_eq!(splits.len(), 3);
    // Verify the labels alternate as expected.
    assert_eq!(splits[0]["topic_before"], "auth");
    assert_eq!(splits[0]["topic_after"], "database");
    assert_eq!(splits[1]["topic_before"], "database");
    assert_eq!(splits[1]["topic_after"], "auth");
    assert_eq!(splits[2]["topic_before"], "auth");
    assert_eq!(splits[2]["topic_after"], "database");
}
