//! Mocked-DB correctness oracle for `find_orphans`.
//!
//! Pins two claims of the tool wrapper:
//!
//! 1. `detail = "files"` → returns the orphan_file_summary rows
//!    verbatim, surfacing every file with `orphan_pct > 0` from the
//!    mock and embedding the correct counts in the output JSON.
//! 2. `detail = "chunks"` → returns the orphan chunk rows verbatim,
//!    surfacing every orphan chunk's content + path.
//!
//! Also asserts the early-out path: when no topic assignments exist,
//! the tool emits the "Run discover_topics first" guidance and does
//! NOT call into the orphan queries.

use std::sync::Arc;

use arc_swap::ArcSwap;
use pgmcp::context::SystemContext;
use pgmcp::db::DbClient;
use pgmcp::db::queries::{OrphanChunkRow, OrphanFileSummary};
use pgmcp::embed::EmbedSource;
use pgmcp::mcp::logging::LogBroadcaster;
use pgmcp::mcp::server::McpServer;
use pgmcp::mcp::tasks::TaskStore;
use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::fixtures::synthetic_corpus::SyntheticCorpus;
use pgmcp_testing::fixtures::test_config;
use pgmcp_testing::mocks::{DeterministicEmbeddingBackend, MockDbClient};
use pgmcp_testing::require_test_db;

mod common;

fn server_with_mock(mock: MockDbClient) -> McpServer {
    let db: Arc<dyn DbClient> = Arc::new(mock);
    let stats = Arc::new(StatsTracker::new());
    let config = Arc::new(ArcSwap::from_pointee(test_config()));
    let log_broadcaster = Arc::new(LogBroadcaster::new());
    let task_store = Arc::new(TaskStore::new());
    let embed_backend: Arc<dyn pgmcp::embed::EmbeddingBackend> =
        Arc::new(DeterministicEmbeddingBackend::new(1024));
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
async fn find_orphans_files_returns_summary_rows_with_pcts() {
    let mut mock = MockDbClient::new();
    mock.has_topic_assignments_result = true;
    mock.orphan_file_summary_result = vec![
        OrphanFileSummary {
            path: "/ws/p/most_orphan.rs".into(),
            project_name: "p".into(),
            language: "rust".into(),
            orphan_chunks: 9,
            total_chunks: 10,
            orphan_pct: 90.0,
        },
        OrphanFileSummary {
            path: "/ws/p/some_orphan.rs".into(),
            project_name: "p".into(),
            language: "rust".into(),
            orphan_chunks: 1,
            total_chunks: 5,
            orphan_pct: 20.0,
        },
    ];
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli("find_orphans", serde_json::json!({"detail": "files"}))
        .await
        .expect("call");
    let payload = text_of(&result);
    let v: serde_json::Value = serde_json::from_str(&payload).expect("json");
    assert_eq!(v["detail"], "files");
    assert_eq!(v["file_count"], 2);
    let files = v["files"].as_array().expect("files array");
    assert_eq!(files.len(), 2);
    let paths: Vec<&str> = files.iter().map(|f| f["path"].as_str().unwrap()).collect();
    assert!(paths.contains(&"/ws/p/most_orphan.rs"));
    assert!(paths.contains(&"/ws/p/some_orphan.rs"));
    // Check the high-orphan-pct file's pct landed.
    let most = files
        .iter()
        .find(|f| f["path"].as_str() == Some("/ws/p/most_orphan.rs"))
        .expect("most orphan");
    assert_eq!(most["orphan_pct"].as_f64(), Some(90.0));
    assert_eq!(most["orphan_chunks"].as_i64(), Some(9));
}

#[tokio::test]
async fn find_orphans_chunks_returns_orphan_chunk_rows_verbatim() {
    let mut mock = MockDbClient::new();
    mock.has_topic_assignments_result = true;
    mock.orphan_chunks_result = vec![
        OrphanChunkRow {
            chunk_id: 100,
            content: "stray helper function".into(),
            path: "/ws/p/stray.rs".into(),
            language: "rust".into(),
            project_name: "p".into(),
            chunk_index: 0,
        },
        OrphanChunkRow {
            chunk_id: 101,
            content: "another orphan".into(),
            path: "/ws/p/stray.rs".into(),
            language: "rust".into(),
            project_name: "p".into(),
            chunk_index: 1,
        },
    ];
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli("find_orphans", serde_json::json!({"detail": "chunks"}))
        .await
        .expect("call");
    let payload = text_of(&result);
    let v: serde_json::Value = serde_json::from_str(&payload).expect("json");
    assert_eq!(v["detail"], "chunks");
    assert_eq!(v["orphan_count"], 2);
    let chunks = v["orphans"].as_array().expect("array");
    assert_eq!(chunks.len(), 2);
    let contents: Vec<&str> = chunks
        .iter()
        .map(|c| c["content"].as_str().unwrap())
        .collect();
    assert!(contents.contains(&"stray helper function"));
    assert!(contents.contains(&"another orphan"));
}

#[tokio::test]
async fn find_orphans_emits_guidance_when_topics_not_yet_computed() {
    let mut mock = MockDbClient::new();
    mock.has_topic_assignments_result = false;
    // Even if mock had orphan rows, the tool should NOT consult them.
    mock.orphan_chunks_result = vec![OrphanChunkRow {
        chunk_id: 999,
        content: "should not appear".into(),
        path: "/ws/p/never.rs".into(),
        language: "rust".into(),
        project_name: "p".into(),
        chunk_index: 0,
    }];
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli("find_orphans", serde_json::json!({"detail": "chunks"}))
        .await
        .expect("call");
    let payload = text_of(&result);
    assert!(
        payload.contains("Run discover_topics first"),
        "expected discover_topics-first guidance, got:\n{payload}"
    );
    assert!(
        !payload.contains("should not appear"),
        "tool must not surface orphan rows when has_topic_assignments=false"
    );
}

#[tokio::test]
async fn find_orphans_rejects_invalid_detail_and_blank_project() {
    let mock = MockDbClient::new();
    let server = server_with_mock(mock);

    let invalid_detail = server
        .call_tool_cli("find_orphans", serde_json::json!({"detail": "summary"}))
        .await
        .expect_err("invalid detail must fail closed");
    assert!(
        invalid_detail.to_string().contains("Unknown detail"),
        "unexpected invalid-detail error: {invalid_detail}"
    );

    let blank_project = server
        .call_tool_cli("find_orphans", serde_json::json!({"project": "   "}))
        .await
        .expect_err("blank project filter must fail closed");
    assert!(
        blank_project
            .to_string()
            .contains("project must be non-empty"),
        "unexpected blank-project error: {blank_project}"
    );
}

#[tokio::test]
async fn find_orphans_real_db_scopes_project_language_and_limit() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    SyntheticCorpus::seed_with_assignments(&pool).await;
    let server = common::server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "find_orphans",
            serde_json::json!({
                "project": " proj-auth ",
                "language": " rust ",
                "detail": "chunks",
                "limit": -10
            }),
        )
        .await
        .expect("find_orphans chunks call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(v["project"], "proj-auth");
    assert_eq!(v["language"], "rust");
    assert_eq!(v["detail"], "chunks");
    assert_eq!(v["limit"], 1);
    assert_eq!(
        v["orphan_count"], 1,
        "negative limit should clamp to 1: {v}"
    );
    let orphans = v["orphans"].as_array().expect("orphans");
    assert!(
        orphans
            .iter()
            .all(|row| row["project_name"] == "proj-auth" && row["language"] == "rust"),
        "orphan chunks must satisfy scoped filters: {v}"
    );
}

#[tokio::test]
async fn find_orphans_rejects_duplicate_project_display_names() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    SyntheticCorpus::seed_with_assignments(&pool).await;
    sqlx::query("INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)")
        .bind("/ws/orphan-dup-a")
        .bind("/ws/orphan-dup-a/orphan-dup")
        .bind("orphan-dup")
        .execute(&pool)
        .await
        .expect("insert first duplicate project");
    sqlx::query("INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)")
        .bind("/ws/orphan-dup-b")
        .bind("/ws/orphan-dup-b/orphan-dup")
        .bind("orphan-dup")
        .execute(&pool)
        .await
        .expect("insert second duplicate project");
    let server = common::server_with_pool(pool);

    let err = server
        .call_tool_cli("find_orphans", serde_json::json!({"project": "orphan-dup"}))
        .await
        .expect_err("duplicate project display names must fail closed");
    assert!(
        err.to_string().contains("ambiguous project name"),
        "unexpected duplicate-project error: {err}"
    );
}
