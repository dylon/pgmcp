//! Mocked-DB correctness oracle for `find_misplaced_code`.
//!
//! The tool computes a per-file mismatch score:
//!
//!   mismatch_score = 1 - (file_topic_count / total_files_in_dir)
//!
//! where `file_topic_count` is the number of files in the same
//! directory whose dominant topic equals the candidate file's
//! dominant topic. Below `min_mismatch` (default 0.5) the file is
//! suppressed.
//!
//! These oracle tests pin three specific outcomes from a small,
//! hand-traced fixture:
//!
//! 1. A file whose topic clashes with its directory majority is
//!    flagged with the expected score.
//! 2. A file in a single-file directory is suppressed (mismatch is
//!    undefined) regardless of topic.
//! 3. Misplaced files come back sorted by mismatch_score descending.

use std::sync::Arc;

use arc_swap::ArcSwap;
use pgmcp::context::SystemContext;
use pgmcp::db::DbClient;
use pgmcp::db::queries::FileTopicRow;
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

fn row(path: &str, topic_id: i32, label: &str) -> FileTopicRow {
    FileTopicRow {
        path: path.into(),
        project_name: "p".into(),
        topic_label: label.into(),
        topic_id,
        chunks_in_topic: 1,
    }
}

#[tokio::test]
async fn find_misplaced_code_flags_lone_misfit_in_directory() {
    // Directory `auth/` has 4 files. 3 are auth-topic, 1 is logging-topic.
    // For the logging-topic file: file_topic_count=1, total=4, mismatch = 0.75.
    // For each auth-topic file: file_topic_count=3, total=4, mismatch = 0.25.
    // With min_mismatch=0.5 (default) only the logging file should be flagged.
    let mut mock = MockDbClient::new();
    mock.chunk_topic_assignments_for_files = vec![
        row("/ws/p/auth/a.rs", 1, "auth"),
        row("/ws/p/auth/b.rs", 1, "auth"),
        row("/ws/p/auth/c.rs", 1, "auth"),
        row("/ws/p/auth/log_misplaced.rs", 2, "logging"),
    ];
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli("find_misplaced_code", serde_json::json!({"project": "p"}))
        .await
        .expect("call");
    let payload = text_of(&result);
    let v: serde_json::Value = serde_json::from_str(&payload).expect("json");
    let misplaced = v["misplaced_files"].as_array().expect("array");
    assert_eq!(misplaced.len(), 1, "expected exactly 1 misplaced file");
    let item = &misplaced[0];
    assert_eq!(item["path"].as_str(), Some("/ws/p/auth/log_misplaced.rs"));
    assert_eq!(item["file_topic"].as_str(), Some("logging"));
    assert_eq!(item["directory_majority_topic"].as_str(), Some("auth"));
    let score: f64 = item["mismatch_score"]
        .as_str()
        .unwrap()
        .parse()
        .expect("parse");
    assert!(
        (score - 0.75).abs() < 1e-3,
        "mismatch_score = {score}, expected 0.75"
    );
}

#[tokio::test]
async fn find_misplaced_code_suppresses_single_file_directories() {
    // Single file in its directory → tool short-circuits (mismatch
    // can't be defined) — must return zero misplaced files.
    let mut mock = MockDbClient::new();
    mock.chunk_topic_assignments_for_files = vec![row("/ws/p/solo/only.rs", 1, "auth")];
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli("find_misplaced_code", serde_json::json!({"project": "p"}))
        .await
        .expect("call");
    let payload = text_of(&result);
    let v: serde_json::Value = serde_json::from_str(&payload).expect("json");
    assert_eq!(v["misplaced_count"], 0);
}

#[tokio::test]
async fn find_misplaced_code_orders_results_by_mismatch_descending() {
    // Set up two directories where two files (in different dirs) are
    // misplaced with different mismatch scores. The tool must rank the
    // higher-mismatch file first.
    //
    // dir A: 1 misfit (logging) + 4 auth → mismatch 0.80 for the misfit;
    //        auth files have mismatch 0.20 (below threshold).
    // dir B: 1 misfit (auth)    + 3 db   → mismatch 0.75 for the misfit;
    //        db files have mismatch 0.25 (below threshold).
    let mut mock = MockDbClient::new();
    mock.chunk_topic_assignments_for_files = vec![
        // dir A
        row("/ws/p/dir_a/auth1.rs", 1, "auth"),
        row("/ws/p/dir_a/auth2.rs", 1, "auth"),
        row("/ws/p/dir_a/auth3.rs", 1, "auth"),
        row("/ws/p/dir_a/auth4.rs", 1, "auth"),
        row("/ws/p/dir_a/big_misfit.rs", 2, "logging"),
        // dir B
        row("/ws/p/dir_b/db1.rs", 3, "database"),
        row("/ws/p/dir_b/db2.rs", 3, "database"),
        row("/ws/p/dir_b/db3.rs", 3, "database"),
        row("/ws/p/dir_b/small_misfit.rs", 1, "auth"),
    ];
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli(
            "find_misplaced_code",
            serde_json::json!({"project": "p", "min_mismatch": 0.4}),
        )
        .await
        .expect("call");
    let payload = text_of(&result);
    let v: serde_json::Value = serde_json::from_str(&payload).expect("json");
    let misplaced = v["misplaced_files"].as_array().expect("array");
    assert_eq!(misplaced.len(), 2, "expected 2 misplaced files");
    assert_eq!(
        misplaced[0]["path"].as_str(),
        Some("/ws/p/dir_a/big_misfit.rs"),
        "highest mismatch must come first"
    );
    assert_eq!(
        misplaced[1]["path"].as_str(),
        Some("/ws/p/dir_b/small_misfit.rs"),
    );
}
