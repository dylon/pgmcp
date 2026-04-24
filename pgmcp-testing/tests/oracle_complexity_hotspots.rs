//! Mocked-DB correctness oracle for `complexity_hotspots`.
//!
//! Pins the composite scoring formula:
//!
//!   composite = 0.30·(chunks/max_chunks)
//!             + 0.25·(topics/max_topics)
//!             + 0.25·(size/max_size)
//!             + 0.20·(coupling/max_coupling)
//!
//! Tests:
//!
//! 1. Hand-computed composite scores match the tool's output for a
//!    pinned 3-file fixture (with a coupled-files mock so the
//!    coupling term is exercised).
//! 2. The `sort_by` parameter switches the ranking to the requested
//!    metric (chunks, size, topics, coupling).

use std::sync::Arc;

use arc_swap::ArcSwap;
use pgmcp::context::SystemContext;
use pgmcp::db::DbClient;
use pgmcp::db::queries::{CoupledFilePair, FileComplexityRow};
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

fn complexity(path: &str, size: i64, chunks: i64, topics: i64) -> FileComplexityRow {
    FileComplexityRow {
        path: path.into(),
        language: "rust".into(),
        size_bytes: size,
        chunk_count: chunks,
        topic_count: topics,
    }
}

#[tokio::test]
async fn complexity_hotspots_composite_matches_hand_computed_formula() {
    // Three files where one is unambiguously the hotspot.
    //   biggest.rs:  size 1000, chunks 20, topics 5
    //   middle.rs:   size  500, chunks 10, topics 3
    //   smallest.rs: size  100, chunks  2, topics 1
    // No coupling data → coupling term is 0 for every file.
    //
    // Normalisation maxima: max_size=1000, max_chunks=20, max_topics=5.
    //   biggest:  0.30*1.0 + 0.25*1.0 + 0.25*1.0 + 0 = 0.80
    //   middle:   0.30*0.5 + 0.25*0.6 + 0.25*0.5 + 0 = 0.425
    //   smallest: 0.30*0.1 + 0.25*0.2 + 0.25*0.1 + 0 = 0.105
    let mut mock = MockDbClient::new();
    mock.file_complexity_data = vec![
        complexity("/ws/p/biggest.rs", 1000, 20, 5),
        complexity("/ws/p/middle.rs", 500, 10, 3),
        complexity("/ws/p/smallest.rs", 100, 2, 1),
    ];
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli("complexity_hotspots", serde_json::json!({"project": "p"}))
        .await
        .expect("call");
    let payload = text_of(&result);
    let v: serde_json::Value = serde_json::from_str(&payload).expect("json");
    let hot = v["hotspots"].as_array().expect("hotspots");
    assert_eq!(hot.len(), 3);

    // Order must be biggest → middle → smallest.
    let order: Vec<&str> = hot.iter().map(|h| h["path"].as_str().unwrap()).collect();
    assert_eq!(
        order,
        vec!["/ws/p/biggest.rs", "/ws/p/middle.rs", "/ws/p/smallest.rs"]
    );

    // Spot-check the composite scores numerically.
    let by_path: std::collections::HashMap<&str, f64> = hot
        .iter()
        .map(|h| {
            (
                h["path"].as_str().unwrap(),
                h["composite_score"]
                    .as_str()
                    .unwrap()
                    .parse::<f64>()
                    .expect("parse"),
            )
        })
        .collect();
    assert!(
        (by_path["/ws/p/biggest.rs"] - 0.80).abs() < 1e-3,
        "biggest composite = {}",
        by_path["/ws/p/biggest.rs"]
    );
    assert!(
        (by_path["/ws/p/middle.rs"] - 0.425).abs() < 1e-3,
        "middle composite = {}",
        by_path["/ws/p/middle.rs"]
    );
    assert!(
        (by_path["/ws/p/smallest.rs"] - 0.105).abs() < 1e-3,
        "smallest composite = {}",
        by_path["/ws/p/smallest.rs"]
    );
}

#[tokio::test]
async fn complexity_hotspots_sort_by_size_overrides_composite() {
    // Hotspot by composite = `chunks_king` (high chunks); hotspot by
    // size = `size_king`. With `sort_by = "size"`, size_king must
    // top the result list.
    let mut mock = MockDbClient::new();
    mock.file_complexity_data = vec![
        complexity("/ws/p/chunks_king.rs", 100, 50, 1),
        complexity("/ws/p/size_king.rs", 5000, 5, 1),
    ];
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli(
            "complexity_hotspots",
            serde_json::json!({"project": "p", "sort_by": "size"}),
        )
        .await
        .expect("call");
    let payload = text_of(&result);
    let v: serde_json::Value = serde_json::from_str(&payload).expect("json");
    let hot = v["hotspots"].as_array().expect("hotspots");
    assert_eq!(hot[0]["path"], "/ws/p/size_king.rs");
}

#[tokio::test]
async fn complexity_hotspots_uses_coupling_data_when_available() {
    // Single file with high coupling — the coupling term should kick
    // in. Without coupling data the coupling subtotal would be 0; with
    // one max-coupling neighbour it should reach 0.20·(1.0/1.0)=0.20.
    let mut mock = MockDbClient::new();
    mock.file_complexity_data = vec![complexity("/ws/p/lone.rs", 100, 1, 1)];
    mock.coupled_files_result = vec![CoupledFilePair {
        file_a: "/ws/p/lone.rs".into(),
        file_b: "/ws/p/other.rs".into(),
        co_commits: 5,
        commits_a: 5,
        commits_b: 5,
        jaccard: 1.0,
    }];
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli("complexity_hotspots", serde_json::json!({"project": "p"}))
        .await
        .expect("call");
    let payload = text_of(&result);
    let v: serde_json::Value = serde_json::from_str(&payload).expect("json");
    let hot = v["hotspots"].as_array().expect("hotspots");
    let entry = &hot[0];
    let max_coupling: f64 = entry["max_coupling"]
        .as_str()
        .unwrap()
        .parse()
        .expect("parse");
    assert!(
        (max_coupling - 1.0).abs() < 1e-3,
        "max_coupling = {max_coupling}, expected 1.0"
    );
    assert_eq!(entry["coupled_files"].as_i64(), Some(1));
    let composite: f64 = entry["composite_score"]
        .as_str()
        .unwrap()
        .parse()
        .expect("parse");
    // chunks=1=max → 0.30; topics=1=max → 0.25; size=100=max → 0.25;
    // coupling 1.0 / 1.0 → 0.20·1 = 0.20. Sum = 1.0.
    assert!(
        (composite - 1.0).abs() < 1e-3,
        "composite = {composite}, expected 1.0"
    );
}
