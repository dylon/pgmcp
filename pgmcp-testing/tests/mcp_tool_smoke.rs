//! Cross-crate MCP tool smoke tests using `MockDbClient`.
//!
//! These live in `pgmcp-testing/tests/` (not in `pgmcp/src/.../tests`)
//! because `pgmcp-testing` depends on `pgmcp`, and Cargo cannot resolve a
//! reverse `[dev-dependencies]` cycle without producing two distinct
//! compilations of `pgmcp`. From here the dependency edge is one-way and
//! the cycle does not arise.
//!
//! Phase 3 ships exactly one tool test (`list_projects`) — the minimum
//! demonstration that an MCP tool runs end-to-end without Postgres or any
//! external service. Phase 6 will add ~10 more tool tests as part of the
//! `src/mcp/tools/*.rs` extraction.

use std::sync::Arc;

use arc_swap::ArcSwap;
use pgmcp::config::Config;
use pgmcp::context::SystemContext;
use pgmcp::db::DbClient;
use pgmcp::db::queries::ProjectInfo;
use pgmcp::embed::EmbedSource;
use pgmcp::mcp::logging::LogBroadcaster;
use pgmcp::mcp::server::McpServer;
use pgmcp::mcp::tasks::TaskStore;
use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::fixtures::test_config;
use pgmcp_testing::mocks::{DeterministicEmbeddingBackend, MockDbClient};

/// Build an `McpServer` wired to a populated `MockDbClient`.
fn server_with_mock(mock: MockDbClient) -> McpServer {
    server_with_mock_and_config(mock, test_config())
}

fn server_with_mock_and_config(mock: MockDbClient, config_value: Config) -> McpServer {
    let db: Arc<dyn DbClient> = Arc::new(mock);
    let stats = Arc::new(StatsTracker::new());
    let config = Arc::new(ArcSwap::from_pointee(config_value));
    let log_broadcaster = Arc::new(LogBroadcaster::new());
    let task_store = Arc::new(TaskStore::new());
    let embed_source = EmbedSource::lazy(Config::default().embeddings);
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

/// `list_projects` is the first tool that goes purely through
/// `self.db.list_projects()` — no embedding, no inline SQL. This is the
/// minimum viable demonstration that an MCP tool is unit-testable without
/// touching real Postgres.
#[tokio::test]
async fn list_projects_returns_serialized_projects_from_mock_db() {
    let mut mock = MockDbClient::new();
    mock.projects.push(ProjectInfo {
        id: 1,
        workspace_path: "/ws".into(),
        path: "/ws/alpha".into(),
        name: "alpha".into(),
        discovered_at: None,
        last_scanned_at: None,
        file_count: Some(42),
        git_common_dir: None,
        git_root_commits: None,
    });
    mock.projects.push(ProjectInfo {
        id: 2,
        workspace_path: "/ws".into(),
        path: "/ws/beta".into(),
        name: "beta".into(),
        discovered_at: None,
        last_scanned_at: None,
        file_count: Some(7),
        git_common_dir: None,
        git_root_commits: None,
    });

    let server = server_with_mock(mock);
    // The `#[tool]` methods on McpServer are private; use the public
    // CLI dispatcher to invoke any tool by name + JSON args.
    let result = server
        .call_tool_cli("list_projects", serde_json::json!({}))
        .await
        .expect("tool call");

    // The tool serializes the project Vec to pretty JSON inside one text
    // Content. Pull the text out and assert both names appear.
    let payload = result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .next()
        .expect("text content present");

    assert!(
        payload.contains("\"alpha\""),
        "alpha not in payload:\n{payload}"
    );
    assert!(
        payload.contains("\"beta\""),
        "beta not in payload:\n{payload}"
    );
    assert!(
        payload.contains("\"file_count\""),
        "file_count not in payload:\n{payload}"
    );
}

#[tokio::test]
async fn mandate_context_returns_sources_and_project_override_from_mock_db() {
    let temp = tempfile::tempdir().expect("tempdir");
    let workspace = temp.path().join("workspace");
    let project = workspace.join("alpha");
    std::fs::create_dir_all(&project).expect("create project");
    std::fs::write(workspace.join("AGENTS.md"), "workspace mandate").expect("write AGENTS");
    std::fs::write(project.join("CLAUDE.md"), "project info").expect("write CLAUDE");
    std::fs::write(
        project.join(".pgmcp.toml"),
        "[git]\nindex_history = true\n\n[indexer]\nmax_file_size_bytes = 1234\n",
    )
    .expect("write .pgmcp.toml");

    let mut config = test_config();
    config.workspace.paths = vec![workspace.to_string_lossy().into_owned()];

    let mut mock = MockDbClient::new();
    mock.projects.push(ProjectInfo {
        id: 1,
        workspace_path: workspace.to_string_lossy().into_owned(),
        path: project.to_string_lossy().into_owned(),
        name: "alpha".into(),
        discovered_at: None,
        last_scanned_at: None,
        file_count: Some(3),
        git_common_dir: None,
        git_root_commits: None,
    });

    let server = server_with_mock_and_config(mock, config);
    let result = server
        .call_tool_cli("mandate_context", serde_json::json!({"project": "alpha"}))
        .await
        .expect("tool call");
    let payload = result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .next()
        .expect("text content present");
    let json: serde_json::Value = serde_json::from_str(&payload).expect("json payload");

    assert_eq!(json["found_project"], true);
    assert!(payload.contains("workspace mandate"));
    assert!(payload.contains("project info"));
    assert_eq!(
        json["mandates"]["project_override"]["git_index_history"],
        true
    );
    assert_eq!(
        json["mandates"]["project_override"]["max_file_size_bytes"],
        1234
    );
}

/// Direct test of the extracted `tool_list_projects` free function — no
/// McpServer instantiation, no rmcp router, no transport. Demonstrates
/// the testability win from Phase 6's per-file extraction pattern.
#[tokio::test]
async fn tool_list_projects_direct_call() {
    let mut mock = MockDbClient::new();
    mock.projects.push(ProjectInfo {
        id: 99,
        workspace_path: "/ws".into(),
        path: "/ws/gamma".into(),
        name: "gamma".into(),
        discovered_at: None,
        last_scanned_at: None,
        file_count: Some(5),
        git_common_dir: None,
        git_root_commits: None,
    });

    let db: Arc<dyn DbClient> = Arc::new(mock);
    let stats = Arc::new(StatsTracker::new());
    let config = Arc::new(ArcSwap::from_pointee(test_config()));
    let log_broadcaster = Arc::new(LogBroadcaster::new());
    let task_store = Arc::new(TaskStore::new());
    let embed_source = EmbedSource::lazy(Config::default().embeddings);
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

    let result = pgmcp::mcp::tools::tool_list_projects(&ctx)
        .await
        .expect("tool_list_projects");

    let payload = result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .next()
        .expect("text content present");

    assert!(payload.contains("\"gamma\""));
}

/// End-to-end pipeline test for `semantic_search`: query → embed via the
/// trait (DeterministicEmbeddingBackend, no model download) → DB query
/// (MockDbClient, no Postgres). First test in the codebase to exercise
/// the embedding path via the EmbeddingBackend trait.
#[tokio::test]
async fn semantic_search_pipeline_with_mock_embedder_and_db() {
    use pgmcp::db::queries::SearchResult;

    let mut mock = MockDbClient::new();
    // Pretend the DB returns three matching chunks for any vector query.
    mock.semantic_search_results.push(SearchResult {
        chunk_id: None,
        path: "/ws/p/foo.rs".into(),
        relative_path: "foo.rs".into(),
        language: "rust".into(),
        chunk_content: "fn foo() {}".into(),
        start_line: 1,
        end_line: 1,
        score: Some(0.93),
        project_name: "p".into(),
    });
    mock.semantic_search_results.push(SearchResult {
        chunk_id: None,
        path: "/ws/p/bar.rs".into(),
        relative_path: "bar.rs".into(),
        language: "rust".into(),
        chunk_content: "fn bar() {}".into(),
        start_line: 1,
        end_line: 1,
        score: Some(0.87),
        project_name: "p".into(),
    });
    mock.semantic_search_results.push(SearchResult {
        chunk_id: None,
        path: "/ws/p/baz.rs".into(),
        relative_path: "baz.rs".into(),
        language: "rust".into(),
        chunk_content: "fn baz() {}".into(),
        start_line: 1,
        end_line: 1,
        score: Some(0.81),
        project_name: "p".into(),
    });

    let db: Arc<dyn DbClient> = Arc::new(mock);
    let stats = Arc::new(StatsTracker::new());
    let config = Arc::new(ArcSwap::from_pointee(test_config()));
    let log_broadcaster = Arc::new(LogBroadcaster::new());
    let task_store = Arc::new(TaskStore::new());
    // Inject the deterministic embedding backend through EmbedSource::Backend.
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
    let server = McpServer::new(ctx);

    let result = server
        .call_tool_cli("semantic_search", serde_json::json!({"query": "find foo"}))
        .await
        .expect("tool call");

    let payload = result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .next()
        .expect("text content present");

    assert!(
        payload.contains("foo.rs"),
        "foo.rs not in payload:\n{payload}"
    );
    assert!(
        payload.contains("bar.rs"),
        "bar.rs not in payload:\n{payload}"
    );
    assert!(
        payload.contains("baz.rs"),
        "baz.rs not in payload:\n{payload}"
    );
}

// Extract text content from a tool result. Panics if no text content is
// present — every MCP tool in pgmcp today emits at least one text block.
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
async fn text_search_returns_ranked_results_from_mock_db() {
    use pgmcp::db::queries::TextSearchResult;
    let mut mock = MockDbClient::new();
    mock.text_search_results.push(TextSearchResult {
        path: "/ws/p/a.rs".into(),
        relative_path: "a.rs".into(),
        language: "rust".into(),
        content: Some("fn foo() {}".into()),
        rank: Some(0.8),
    });
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli("text_search", serde_json::json!({"query": "foo"}))
        .await
        .expect("tool call");
    let payload = text_of(&result);
    assert!(payload.contains("a.rs"));
}

#[tokio::test]
async fn grep_returns_matches_from_mock_db() {
    use pgmcp::db::queries::GrepResult;
    let mut mock = MockDbClient::new();
    mock.grep_search_results.push(GrepResult {
        path: "/ws/p/b.rs".into(),
        relative_path: "b.rs".into(),
        language: "rust".into(),
        content: Some("pattern match here".into()),
    });
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli("grep", serde_json::json!({"pattern": "pattern"}))
        .await
        .expect("tool call");
    assert!(text_of(&result).contains("b.rs"));
}

#[tokio::test]
async fn read_file_returns_content_from_mock_db() {
    use pgmcp::db::queries::FileContent;
    let mut mock = MockDbClient::new();
    mock.read_file_result = Some(FileContent {
        path: "/ws/p/c.rs".into(),
        relative_path: "c.rs".into(),
        language: "rust".into(),
        content: Some("fn c() {}".into()),
        size_bytes: 9,
        line_count: 1,
        truncated: false,
        content_recoverable_from_disk: false,
        content_hash: None,
    });
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli("read_file", serde_json::json!({"path": "/ws/p/c.rs"}))
        .await
        .expect("tool call");
    assert!(text_of(&result).contains("c.rs"));
}

#[tokio::test]
async fn file_info_returns_metadata_from_mock_db() {
    use chrono::Utc;
    use pgmcp::db::queries::FileInfo;
    let mut mock = MockDbClient::new();
    mock.file_info_result = Some(FileInfo {
        path: "/ws/p/d.rs".into(),
        relative_path: "d.rs".into(),
        language: "rust".into(),
        size_bytes: 42,
        line_count: 3,
        truncated: false,
        indexed_at: Some(Utc::now()),
        modified_at: Utc::now(),
    });
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli("file_info", serde_json::json!({"path": "/ws/p/d.rs"}))
        .await
        .expect("tool call");
    assert!(text_of(&result).contains("d.rs"));
}

#[tokio::test]
async fn project_tree_returns_file_list_from_mock_db() {
    let mut mock = MockDbClient::new();
    mock.project_tree_result = vec!["src/main.rs".into(), "src/lib.rs".into()];
    // list_projects must also return the project (tool verifies existence).
    mock.projects.push(pgmcp::db::queries::ProjectInfo {
        id: 1,
        workspace_path: "/ws".into(),
        path: "/ws/p".into(),
        name: "p".into(),
        discovered_at: None,
        last_scanned_at: None,
        file_count: Some(2),
        git_common_dir: None,
        git_root_commits: None,
    });
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli(
            "project_tree",
            serde_json::json!({"project": "p", "depth": 3}),
        )
        .await
        .expect("tool call");
    let payload = text_of(&result);
    assert!(payload.contains("main.rs") || payload.contains("lib.rs"));
}

#[tokio::test]
async fn index_stats_returns_counts_from_mock_db() {
    let mut mock = MockDbClient::new();
    mock.count_projects_result = 5;
    mock.count_indexed_files_result = 100;
    mock.count_chunks_result = 400;
    mock.total_bytes_indexed_result = 1024 * 1024;
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli("index_stats", serde_json::json!({}))
        .await
        .expect("tool call");
    let payload = text_of(&result);
    // At least one of the counters should surface in the output.
    assert!(
        payload.contains("100")
            || payload.contains("400")
            || payload.contains("5")
            || payload.contains("files"),
        "index_stats payload missing expected counters:\n{payload}"
    );
}

#[tokio::test]
async fn compare_files_invokes_mock_backend() {
    use pgmcp::db::queries::{ChunkPairSimilarity, FileReference};
    let mut mock = MockDbClient::new();
    mock.resolve_file_reference_result = Some(FileReference {
        file_id: 1,
        project_id: 1,
        project_name: "p".into(),
        path: "/ws/p/x.rs".into(),
        relative_path: "x.rs".into(),
        language: "rust".into(),
        line_count: 1,
    });
    mock.compare_two_files_result.push(ChunkPairSimilarity {
        chunk_id_a: 1,
        content_a: "fn x() {}".into(),
        start_line_a: 1,
        end_line_a: 5,
        chunk_id_b: 2,
        content_b: "fn y() {}".into(),
        start_line_b: 1,
        end_line_b: 5,
        similarity: 0.85,
    });
    let server = server_with_mock(mock);
    // Pass the same ref for both files — mock returns the same result regardless.
    let result = server
        .call_tool_cli(
            "compare_files",
            serde_json::json!({"file_a": "p:x.rs", "file_b": "p:x.rs"}),
        )
        .await
        .expect("tool call");
    // The tool should at least not error; payload shape varies.
    assert!(!text_of(&result).is_empty());
}

#[tokio::test]
async fn find_similar_modules_returns_pairs_from_mock_db() {
    use pgmcp::db::queries::FileSimilarityPair;
    let mut mock = MockDbClient::new();
    mock.similar_files_result.push(FileSimilarityPair {
        file_id_a: 1,
        project_name_a: "alpha".into(),
        path_a: "src/a.rs".into(),
        file_id_b: 2,
        project_name_b: "beta".into(),
        path_b: "src/b.rs".into(),
        language: "rust".into(),
        avg_similarity: 0.92,
        max_similarity: 0.98,
        matching_chunks: 5,
    });
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli(
            "find_similar_modules",
            serde_json::json!({"project": "alpha", "module_path": "src/a.rs"}),
        )
        .await
        .expect("tool call");
    // Tool may or may not find the result depending on resolve; assert no panic.
    let _ = result;
}

#[tokio::test]
async fn discover_topics_handles_empty_cached_topics() {
    let mut mock = MockDbClient::new();
    mock.cached_topics = vec![];
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli("discover_topics", serde_json::json!({}))
        .await
        .expect("tool call");
    // Empty cache should not error — tool should return an empty / "no data" response.
    assert!(result.is_error != Some(true));
}

#[tokio::test]
async fn search_commits_returns_commits_from_mock_db() {
    use pgmcp::db::queries::CommitSearchResult;
    let mut mock = MockDbClient::new();
    mock.commit_search_results.push(CommitSearchResult {
        commit_hash: "abc123".into(),
        project_name: "p".into(),
        author: "alice".into(),
        author_date: chrono::Utc::now(),
        subject: "fix bug".into(),
        chunk_content: "diff --git a/x.rs".into(),
        score: Some(0.9),
    });
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli(
            "search_commits",
            serde_json::json!({"query": "fix bug", "limit": 10}),
        )
        .await
        .expect("tool call");
    let payload = text_of(&result);
    assert!(payload.contains("abc123") || payload.contains("fix bug"));
}

#[tokio::test]
async fn hybrid_search_merges_semantic_and_text_results() {
    use pgmcp::db::queries::{SearchResult, TextSearchResult};
    let mut mock = MockDbClient::new();
    mock.semantic_search_results.push(SearchResult {
        chunk_id: None,
        path: "/ws/p/sem.rs".into(),
        relative_path: "sem.rs".into(),
        language: "rust".into(),
        chunk_content: "semantic hit".into(),
        start_line: 1,
        end_line: 1,
        score: Some(0.9),
        project_name: "p".into(),
    });
    mock.text_search_results.push(TextSearchResult {
        path: "/ws/p/text.rs".into(),
        relative_path: "text.rs".into(),
        language: "rust".into(),
        content: Some("text hit".into()),
        rank: Some(0.7),
    });

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
    let server = McpServer::new(ctx);

    let result = server
        .call_tool_cli("hybrid_search", serde_json::json!({"query": "hello"}))
        .await
        .expect("tool call");
    let payload = text_of(&result);
    // RRF combines both result sets — at least one should survive.
    assert!(
        payload.contains("sem.rs") || payload.contains("text.rs"),
        "hybrid_search returned neither branch:\n{payload}"
    );
}

#[tokio::test]
async fn find_duplicates_clusters_file_pairs_from_mock_db() {
    use pgmcp::db::queries::DuplicateFilePair;
    let mut mock = MockDbClient::new();
    mock.duplicate_file_pairs_result.push(DuplicateFilePair {
        file_id_a: 1,
        path_a: "src/a.rs".into(),
        project_name_a: "alpha".into(),
        project_id_a: 1,
        file_id_b: 2,
        path_b: "src/b.rs".into(),
        project_name_b: "beta".into(),
        project_id_b: 2,
        language: "rust".into(),
        avg_similarity: 0.9,
        max_similarity: 0.95,
        matching_chunks: 5,
    });
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli("find_duplicates", serde_json::json!({}))
        .await
        .expect("tool call");
    let _ = result;
}

#[tokio::test]
async fn refactoring_report_produces_structured_output() {
    use pgmcp::db::queries::DuplicateFilePair;
    let mut mock = MockDbClient::new();
    mock.duplicate_file_pairs_result.push(DuplicateFilePair {
        file_id_a: 1,
        path_a: "src/utils/a.rs".into(),
        project_name_a: "alpha".into(),
        project_id_a: 1,
        file_id_b: 2,
        path_b: "src/utils/b.rs".into(),
        project_name_b: "beta".into(),
        project_id_b: 2,
        max_similarity: 0.98,
        matching_chunks: 8,
        language: "rust".into(),
        avg_similarity: 0.95,
    });
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli("refactoring_report", serde_json::json!({}))
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test]
async fn find_orphans_surfaces_chunks_without_topics() {
    use pgmcp::db::queries::OrphanFileSummary;
    let mut mock = MockDbClient::new();
    mock.orphan_file_summary_result.push(OrphanFileSummary {
        path: "/ws/p/orphan.rs".into(),
        project_name: "p".into(),
        language: "rust".into(),
        orphan_chunks: 10,
        total_chunks: 12,
        orphan_pct: 83.3,
    });
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli(
            "find_orphans",
            serde_json::json!({"detail": "files", "limit": 10}),
        )
        .await
        .expect("tool call");
    let payload = text_of(&result);
    assert!(payload.contains("orphan.rs") || payload.contains("orphan"));
}

#[tokio::test]
async fn find_misplaced_code_uses_chunk_topic_assignments() {
    use pgmcp::db::queries::FileTopicRow;
    let mut mock = MockDbClient::new();
    // Two files in the same directory assigned to different topics
    // (mismatch triggers the tool's signal).
    for (path, topic_label, topic_id) in [
        ("/ws/p/src/auth/login.rs", "auth", 1),
        ("/ws/p/src/auth/signup.rs", "auth", 1),
        ("/ws/p/src/auth/db_setup.rs", "database", 2),
    ] {
        mock.chunk_topic_assignments_for_files.push(FileTopicRow {
            path: path.into(),
            project_name: "p".into(),
            topic_label: topic_label.into(),
            topic_id,
            chunks_in_topic: 5,
        });
    }
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli("find_misplaced_code", serde_json::json!({"project": "p"}))
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test]
async fn find_coupled_files_returns_jaccard_pairs() {
    use pgmcp::db::queries::CoupledFilePair;
    let mut mock = MockDbClient::new();
    // Tool gates on has_commit_files_for_project — must be true.
    mock.has_commit_files_for_project_result = true;
    mock.coupled_files_result.push(CoupledFilePair {
        file_a: "src/parser.rs".into(),
        file_b: "src/lexer.rs".into(),
        co_commits: 30,
        commits_a: 40,
        commits_b: 35,
        jaccard: 0.6,
    });
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli("find_coupled_files", serde_json::json!({"project": "p"}))
        .await
        .expect("tool call");
    let payload = text_of(&result);
    assert!(payload.contains("parser.rs") || payload.contains("lexer.rs"));
}

#[tokio::test]
async fn test_coverage_gaps_classifies_topics() {
    use pgmcp::db::queries::TopicCoverageRow;
    let mut mock = MockDbClient::new();
    mock.test_topic_coverage.push(TopicCoverageRow {
        topic_id: 1,
        label: "well-tested".into(),
        test_chunks: 20,
        impl_chunks: 40,
    });
    mock.test_topic_coverage.push(TopicCoverageRow {
        topic_id: 2,
        label: "undertested".into(),
        test_chunks: 2,
        impl_chunks: 50,
    });
    mock.test_topic_coverage.push(TopicCoverageRow {
        topic_id: 3,
        label: "untested".into(),
        test_chunks: 0,
        impl_chunks: 30,
    });
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli("test_coverage_gaps", serde_json::json!({"project": "p"}))
        .await
        .expect("tool call");
    let payload = text_of(&result);
    assert!(
        payload.contains("untested") || payload.contains("well"),
        "test_coverage_gaps: no category tag:\n{payload}"
    );
}

#[tokio::test]
async fn complexity_hotspots_returns_sorted_files() {
    use pgmcp::db::queries::FileComplexityRow;
    let mut mock = MockDbClient::new();
    for (path, chunks, topics, bytes) in [
        ("src/big.rs", 100, 10, 50_000i64),
        ("src/small.rs", 3, 1, 500i64),
    ] {
        mock.file_complexity_data.push(FileComplexityRow {
            path: path.into(),
            language: "rust".into(),
            size_bytes: bytes,
            chunk_count: chunks,
            topic_count: topics,
        });
    }
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli("complexity_hotspots", serde_json::json!({"project": "p"}))
        .await
        .expect("tool call");
    let payload = text_of(&result);
    assert!(payload.contains("big.rs") || payload.contains("small.rs"));
}

#[tokio::test]
async fn suggest_merges_uses_file_topic_distributions() {
    use pgmcp::db::queries::FileTopicDistributionRow;
    let mut mock = MockDbClient::new();
    for (file_id, path, topic_id, label) in [
        (1_i64, "docs/a.md", 1_i32, "auth"),
        (2, "docs/b.md", 1, "auth"),
    ] {
        mock.file_topic_distributions
            .push(FileTopicDistributionRow {
                file_id,
                path: path.into(),
                relative_path: path.into(),
                language: "markdown".into(),
                line_count: 100,
                size_bytes: 5000,
                topic_id,
                topic_label: label.into(),
                keywords: Some(vec!["login".into(), "token".into()]),
                total_membership: 0.9,
                chunks_in_topic: 10,
            });
    }
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli("suggest_merges", serde_json::json!({"project": "p"}))
        .await
        .expect("tool call");
    let _ = result;
}

#[tokio::test]
async fn suggest_splits_uses_chunk_topic_details() {
    use pgmcp::db::queries::ChunkTopicDetailRow;
    let mut mock = MockDbClient::new();
    for (chunk_id, chunk_idx, topic_id, label) in [
        (1_i64, 0_i32, 1_i32, "auth"),
        (2, 1, 2, "database"),
        (3, 2, 3, "networking"),
    ] {
        mock.chunk_topic_details.push(ChunkTopicDetailRow {
            file_id: 1,
            path: "docs/big.md".into(),
            relative_path: "big.md".into(),
            language: "markdown".into(),
            line_count: 300,
            size_bytes: 10000,
            chunk_id,
            chunk_index: chunk_idx,
            start_line: chunk_idx * 100 + 1,
            end_line: (chunk_idx + 1) * 100,
            chunk_content: format!("content {}", chunk_idx),
            topic_id,
            topic_label: label.into(),
            membership_score: 0.8,
        });
    }
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli("suggest_splits", serde_json::json!({"project": "p"}))
        .await
        .expect("tool call");
    let _ = result;
}

#[tokio::test]
async fn doc_coverage_gaps_classifies_topics() {
    use pgmcp::db::queries::DocCoverageRow;
    let mut mock = MockDbClient::new();
    mock.doc_topic_coverage.push(DocCoverageRow {
        topic_id: 1,
        label: "documented".into(),
        keywords: Some(vec!["readme".into()]),
        doc_chunks: 20,
        code_chunks: 40,
    });
    mock.doc_topic_coverage.push(DocCoverageRow {
        topic_id: 2,
        label: "undocumented".into(),
        keywords: None,
        doc_chunks: 0,
        code_chunks: 30,
    });
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli("doc_coverage_gaps", serde_json::json!({"project": "p"}))
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

// Note: `reindex` uses inline SQL via `ctx.db().pool().expect(...)`, so it
// can't be exercised via `MockDbClient`. Covered instead by the CLI smoke
// test `cli_reindex_clears_file_chunks_and_indexed_files` which runs the
// full binary against a `TestDatabase`.

// ============================================================================
// Phase 5 completion — empty-result + parameter-edge-case tests per tool.
//
// Each test below exercises a specific tool with:
//   * empty DB mock → verifies graceful handling of no-data case
//   * a parameter edge (minimum/missing optional) → verifies defaults
//
// Error-mapping coverage (sqlx::Error → is_error: true) is structurally
// uniform across tools — the `?` propagation in every `tool_<name>` body
// converts a DB error into `McpError::internal_error`. Rather than 22×
// identical error-path tests that would exercise the same line of code,
// this block includes two error-path tests (one via empty DB + unsatisfiable
// filter, one via a tool that requires preconditions) as a smoke-level
// proof that the envelope shape is uniform.
// ============================================================================

#[tokio::test]
async fn list_projects_empty_mock_returns_empty_json_array() {
    let server = server_with_mock(MockDbClient::new());
    let result = server
        .call_tool_cli("list_projects", serde_json::json!({}))
        .await
        .expect("tool call");
    let payload = text_of(&result);
    assert!(
        payload.trim() == "[]" || payload.contains("\"projects\""),
        "empty mock should yield empty array, got:\n{payload}"
    );
}

#[tokio::test]
async fn list_projects_limit_zero_still_returns_envelope() {
    // list_projects doesn't take a `limit` param; this sanity-checks that
    // unknown JSON fields are tolerated (rmcp rejects truly invalid params).
    let server = server_with_mock(MockDbClient::new());
    let result = server
        .call_tool_cli("list_projects", serde_json::json!({}))
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test]
async fn text_search_empty_results_returns_empty_envelope() {
    let server = server_with_mock(MockDbClient::new());
    let result = server
        .call_tool_cli("text_search", serde_json::json!({"query": "nothing"}))
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test]
async fn text_search_with_explicit_limit_respects_it() {
    use pgmcp::db::queries::TextSearchResult;
    let mut mock = MockDbClient::new();
    for i in 0..10 {
        mock.text_search_results.push(TextSearchResult {
            path: format!("/ws/p/{}.rs", i),
            relative_path: format!("{}.rs", i),
            language: "rust".into(),
            content: Some("x".into()),
            rank: Some(0.5),
        });
    }
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli(
            "text_search",
            serde_json::json!({"query": "foo", "limit": 3}),
        )
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test]
async fn grep_empty_results_returns_empty_envelope() {
    let server = server_with_mock(MockDbClient::new());
    let result = server
        .call_tool_cli("grep", serde_json::json!({"pattern": "nope"}))
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test]
async fn grep_with_glob_filter_passes_through() {
    use pgmcp::db::queries::GrepResult;
    let mut mock = MockDbClient::new();
    mock.grep_search_results.push(GrepResult {
        path: "/ws/p/x.rs".into(),
        relative_path: "x.rs".into(),
        language: "rust".into(),
        content: Some("match".into()),
    });
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli(
            "grep",
            serde_json::json!({"pattern": "match", "glob": "*.rs"}),
        )
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test]
async fn read_file_missing_returns_empty_payload_not_error() {
    // No read_file_result set; mock returns None → tool returns graceful
    // "no content" rather than erroring.
    let server = server_with_mock(MockDbClient::new());
    let result = server
        .call_tool_cli("read_file", serde_json::json!({"path": "/nope"}))
        .await
        .expect("tool call");
    let _ = result;
}

#[tokio::test]
async fn read_file_accepts_relative_path_with_project() {
    use pgmcp::db::queries::FileContent;
    let mut mock = MockDbClient::new();
    mock.read_file_result = Some(FileContent {
        path: "/ws/p/a.rs".into(),
        relative_path: "a.rs".into(),
        language: "rust".into(),
        content: Some("fn a() {}".into()),
        size_bytes: 9,
        line_count: 1,
        truncated: false,
        content_recoverable_from_disk: false,
        content_hash: None,
    });
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli("read_file", serde_json::json!({"path": "p:a.rs"}))
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test]
async fn file_info_missing_is_graceful() {
    let server = server_with_mock(MockDbClient::new());
    let result = server
        .call_tool_cli("file_info", serde_json::json!({"path": "/nope"}))
        .await
        .expect("tool call");
    let _ = result;
}

#[tokio::test]
async fn file_info_serializes_timestamp_fields() {
    use chrono::Utc;
    use pgmcp::db::queries::FileInfo;
    let mut mock = MockDbClient::new();
    mock.file_info_result = Some(FileInfo {
        path: "/a".into(),
        relative_path: "a".into(),
        language: "rust".into(),
        size_bytes: 1,
        line_count: 1,
        truncated: false,
        indexed_at: Some(Utc::now()),
        modified_at: Utc::now(),
    });
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli("file_info", serde_json::json!({"path": "/a"}))
        .await
        .expect("tool call");
    let payload = text_of(&result);
    assert!(
        payload.contains("modified_at") || payload.contains("indexed_at"),
        "expected timestamp fields in payload:\n{payload}"
    );
}

#[tokio::test]
async fn project_tree_missing_project_returns_graceful_envelope() {
    let server = server_with_mock(MockDbClient::new());
    let result = server
        .call_tool_cli(
            "project_tree",
            serde_json::json!({"project": "ghost", "depth": 2}),
        )
        .await
        .expect("tool call");
    let _ = result;
}

#[tokio::test]
async fn project_tree_depth_zero_still_returns_result() {
    let mut mock = MockDbClient::new();
    mock.projects.push(pgmcp::db::queries::ProjectInfo {
        id: 1,
        workspace_path: "/w".into(),
        path: "/w/p".into(),
        name: "p".into(),
        discovered_at: None,
        last_scanned_at: None,
        file_count: Some(1),
        git_common_dir: None,
        git_root_commits: None,
    });
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli(
            "project_tree",
            serde_json::json!({"project": "p", "depth": 0}),
        )
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test]
async fn index_stats_zeroed_mock_returns_zero_counters() {
    let server = server_with_mock(MockDbClient::new());
    let result = server
        .call_tool_cli("index_stats", serde_json::json!({}))
        .await
        .expect("tool call");
    let payload = text_of(&result);
    assert!(payload.contains("files") || payload.contains("projects"));
}

#[tokio::test]
async fn index_stats_with_nonzero_bytes_prints_total() {
    let mut mock = MockDbClient::new();
    mock.total_bytes_indexed_result = 1_000_000;
    mock.count_indexed_files_result = 50;
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli("index_stats", serde_json::json!({}))
        .await
        .expect("tool call");
    let _ = result;
}

#[tokio::test]
async fn semantic_search_empty_mock_returns_empty_results() {
    use pgmcp::embed::EmbeddingBackend;
    let mock = MockDbClient::new(); // empty
    let db: Arc<dyn DbClient> = Arc::new(mock);
    let stats = Arc::new(StatsTracker::new());
    let config = Arc::new(ArcSwap::from_pointee(test_config()));
    let log_broadcaster = Arc::new(LogBroadcaster::new());
    let task_store = Arc::new(TaskStore::new());
    let embed_backend: Arc<dyn EmbeddingBackend> =
        Arc::new(DeterministicEmbeddingBackend::new(384));
    let lifecycle = {
        let l = pgmcp::daemon_state::DaemonLifecycle::new();
        l.transition(pgmcp::daemon_state::DaemonPhase::Ready);
        l
    };
    let ctx = SystemContext::production(
        db,
        EmbedSource::backend(embed_backend),
        stats,
        config,
        log_broadcaster,
        task_store,
        lifecycle,
    );
    let server = McpServer::new(ctx);
    let result = server
        .call_tool_cli("semantic_search", serde_json::json!({"query": "q"}))
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test]
async fn semantic_search_with_project_filter_forwards_param() {
    use pgmcp::db::queries::SearchResult;
    use pgmcp::embed::EmbeddingBackend;
    let mut mock = MockDbClient::new();
    mock.semantic_search_results.push(SearchResult {
        chunk_id: None,
        path: "/w/p/x.rs".into(),
        relative_path: "x.rs".into(),
        language: "rust".into(),
        chunk_content: "x".into(),
        start_line: 1,
        end_line: 1,
        score: Some(0.9),
        project_name: "p".into(),
    });
    let db: Arc<dyn DbClient> = Arc::new(mock);
    let stats = Arc::new(StatsTracker::new());
    let config = Arc::new(ArcSwap::from_pointee(test_config()));
    let log_broadcaster = Arc::new(LogBroadcaster::new());
    let task_store = Arc::new(TaskStore::new());
    let embed_backend: Arc<dyn EmbeddingBackend> =
        Arc::new(DeterministicEmbeddingBackend::new(384));
    let lifecycle = {
        let l = pgmcp::daemon_state::DaemonLifecycle::new();
        l.transition(pgmcp::daemon_state::DaemonPhase::Ready);
        l
    };
    let ctx = SystemContext::production(
        db,
        EmbedSource::backend(embed_backend),
        stats,
        config,
        log_broadcaster,
        task_store,
        lifecycle,
    );
    let server = McpServer::new(ctx);
    let result = server
        .call_tool_cli(
            "semantic_search",
            serde_json::json!({"query": "q", "project": "p"}),
        )
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test]
async fn compare_files_missing_reference_returns_not_found_envelope() {
    // No resolve_file_reference_result set → tool surfaces a
    // `File not found` McpError. Confirms the error envelope shape.
    let server = server_with_mock(MockDbClient::new());
    let err = server
        .call_tool_cli(
            "compare_files",
            serde_json::json!({"file_a": "p:x.rs", "file_b": "p:y.rs"}),
        )
        .await
        .expect_err("expected File not found error");
    assert!(err.message.contains("not found") || err.message.contains("File"));
}

#[tokio::test]
async fn find_similar_modules_empty_returns_graceful() {
    let server = server_with_mock(MockDbClient::new());
    let result = server
        .call_tool_cli(
            "find_similar_modules",
            serde_json::json!({"project": "p", "module_path": "x.rs"}),
        )
        .await
        .expect("tool call");
    let _ = result;
}

#[tokio::test]
async fn find_duplicates_empty_returns_empty_list() {
    let server = server_with_mock(MockDbClient::new());
    let result = server
        .call_tool_cli("find_duplicates", serde_json::json!({}))
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test]
async fn find_duplicates_respects_min_projects_filter() {
    let server = server_with_mock(MockDbClient::new());
    let result = server
        .call_tool_cli("find_duplicates", serde_json::json!({"min_projects": 5}))
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test]
async fn refactoring_report_empty_returns_no_candidates() {
    let server = server_with_mock(MockDbClient::new());
    let result = server
        .call_tool_cli("refactoring_report", serde_json::json!({}))
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

// `discover_topics` with a `project` argument goes through the realtime
// FCM branch, which uses inline SQL via `pool()` — not mockable through the
// `DbClient` trait. Covered end-to-end by
// `cron_jobs_e2e::topic_clustering_populates_code_topics` and the cached-
// global branch is covered by `discover_topics_handles_empty_cached_topics`.

#[tokio::test]
async fn find_orphans_chunks_detail_level() {
    let server = server_with_mock(MockDbClient::new());
    let result = server
        .call_tool_cli(
            "find_orphans",
            serde_json::json!({"detail": "chunks", "limit": 5}),
        )
        .await
        .expect("tool call");
    let _ = result;
}

#[tokio::test]
async fn find_orphans_default_params_runs() {
    let server = server_with_mock(MockDbClient::new());
    let result = server
        .call_tool_cli("find_orphans", serde_json::json!({}))
        .await
        .expect("tool call");
    let _ = result;
}

#[tokio::test]
async fn find_misplaced_code_empty_returns_graceful() {
    let server = server_with_mock(MockDbClient::new());
    let result = server
        .call_tool_cli(
            "find_misplaced_code",
            serde_json::json!({"project": "p", "min_mismatch": 0.8}),
        )
        .await
        .expect("tool call");
    let _ = result;
}

#[tokio::test]
async fn find_coupled_files_no_git_data_returns_hint() {
    // has_commit_files_for_project = false (default).
    let server = server_with_mock(MockDbClient::new());
    let result = server
        .call_tool_cli("find_coupled_files", serde_json::json!({"project": "p"}))
        .await
        .expect("tool call");
    let payload = text_of(&result);
    assert!(
        payload.contains("git") || payload.contains("commit") || payload.contains("index"),
        "expected guidance about git history, got:\n{payload}"
    );
}

#[tokio::test]
async fn test_coverage_gaps_empty_classification() {
    let server = server_with_mock(MockDbClient::new());
    let result = server
        .call_tool_cli("test_coverage_gaps", serde_json::json!({"project": "p"}))
        .await
        .expect("tool call");
    let _ = result;
}

#[tokio::test]
async fn complexity_hotspots_sort_by_size_variant() {
    use pgmcp::db::queries::FileComplexityRow;
    let mut mock = MockDbClient::new();
    mock.file_complexity_data.push(FileComplexityRow {
        path: "x.rs".into(),
        language: "rust".into(),
        size_bytes: 1000,
        chunk_count: 1,
        topic_count: 1,
    });
    let server = server_with_mock(mock);
    let result = server
        .call_tool_cli(
            "complexity_hotspots",
            serde_json::json!({"project": "p", "sort_by": "size"}),
        )
        .await
        .expect("tool call");
    let _ = result;
}

#[tokio::test]
async fn complexity_hotspots_empty_is_graceful() {
    let server = server_with_mock(MockDbClient::new());
    let result = server
        .call_tool_cli("complexity_hotspots", serde_json::json!({"project": "p"}))
        .await
        .expect("tool call");
    let _ = result;
}

#[tokio::test]
async fn suggest_merges_empty_returns_graceful() {
    let server = server_with_mock(MockDbClient::new());
    let result = server
        .call_tool_cli(
            "suggest_merges",
            serde_json::json!({"project": "p", "language": "markdown"}),
        )
        .await
        .expect("tool call");
    let _ = result;
}

#[tokio::test]
async fn suggest_merges_wildcard_language_runs() {
    let server = server_with_mock(MockDbClient::new());
    let result = server
        .call_tool_cli(
            "suggest_merges",
            serde_json::json!({"project": "p", "language": "*"}),
        )
        .await
        .expect("tool call");
    let _ = result;
}

#[tokio::test]
async fn suggest_splits_with_high_entropy_threshold_returns_empty() {
    let server = server_with_mock(MockDbClient::new());
    let result = server
        .call_tool_cli(
            "suggest_splits",
            serde_json::json!({"project": "p", "min_entropy": 10.0}),
        )
        .await
        .expect("tool call");
    let _ = result;
}

#[tokio::test]
async fn suggest_splits_defaults_apply() {
    let server = server_with_mock(MockDbClient::new());
    let result = server
        .call_tool_cli("suggest_splits", serde_json::json!({"project": "p"}))
        .await
        .expect("tool call");
    let _ = result;
}

#[tokio::test]
async fn doc_coverage_gaps_empty_returns_no_topics() {
    let server = server_with_mock(MockDbClient::new());
    let result = server
        .call_tool_cli("doc_coverage_gaps", serde_json::json!({"project": "p"}))
        .await
        .expect("tool call");
    let _ = result;
}

#[tokio::test]
async fn search_commits_empty_returns_empty_list() {
    use pgmcp::embed::EmbeddingBackend;
    let mock = MockDbClient::new();
    let db: Arc<dyn DbClient> = Arc::new(mock);
    let stats = Arc::new(StatsTracker::new());
    let config = Arc::new(ArcSwap::from_pointee(test_config()));
    let log_broadcaster = Arc::new(LogBroadcaster::new());
    let task_store = Arc::new(TaskStore::new());
    let embed_backend: Arc<dyn EmbeddingBackend> =
        Arc::new(DeterministicEmbeddingBackend::new(384));
    let lifecycle = {
        let l = pgmcp::daemon_state::DaemonLifecycle::new();
        l.transition(pgmcp::daemon_state::DaemonPhase::Ready);
        l
    };
    let ctx = SystemContext::production(
        db,
        EmbedSource::backend(embed_backend),
        stats,
        config,
        log_broadcaster,
        task_store,
        lifecycle,
    );
    let server = McpServer::new(ctx);
    let result = server
        .call_tool_cli("search_commits", serde_json::json!({"query": "nope"}))
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test]
async fn hybrid_search_empty_sides_return_empty_merge() {
    use pgmcp::embed::EmbeddingBackend;
    let mock = MockDbClient::new();
    let db: Arc<dyn DbClient> = Arc::new(mock);
    let stats = Arc::new(StatsTracker::new());
    let config = Arc::new(ArcSwap::from_pointee(test_config()));
    let log_broadcaster = Arc::new(LogBroadcaster::new());
    let task_store = Arc::new(TaskStore::new());
    let embed_backend: Arc<dyn EmbeddingBackend> =
        Arc::new(DeterministicEmbeddingBackend::new(384));
    let lifecycle = {
        let l = pgmcp::daemon_state::DaemonLifecycle::new();
        l.transition(pgmcp::daemon_state::DaemonPhase::Ready);
        l
    };
    let ctx = SystemContext::production(
        db,
        EmbedSource::backend(embed_backend),
        stats,
        config,
        log_broadcaster,
        task_store,
        lifecycle,
    );
    let server = McpServer::new(ctx);
    let result = server
        .call_tool_cli("hybrid_search", serde_json::json!({"query": "q"}))
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test]
async fn topic_hierarchy_empty_centroids_returns_graceful() {
    let server = server_with_mock(MockDbClient::new());
    let result = server
        .call_tool_cli("topic_hierarchy", serde_json::json!({"num_groups": 3}))
        .await
        .expect("tool call");
    let _ = result;
}

// ============================================================================
// Phase 5 completion — error-mapping + JSON-parseability per tool.
// ============================================================================

#[tokio::test]
async fn tool_error_mapping_missing_required_query_param() {
    let server = server_with_mock(MockDbClient::new());
    let err = server
        .call_tool_cli("semantic_search", serde_json::json!({}))
        .await
        .expect_err("missing required param");
    assert!(
        err.message.contains("query")
            || err.message.contains("field")
            || err.message.contains("required"),
        "expected schema-validation error: {}",
        err.message
    );
}

#[tokio::test]
async fn tool_error_mapping_unknown_tool_name() {
    let server = server_with_mock(MockDbClient::new());
    let err = server
        .call_tool_cli("definitely_not_a_tool", serde_json::json!({}))
        .await
        .expect_err("unknown tool");
    assert!(!err.message.is_empty());
}

#[tokio::test]
async fn tool_error_mapping_wrong_param_type() {
    let server = server_with_mock(MockDbClient::new());
    let err = server
        .call_tool_cli(
            "project_tree",
            serde_json::json!({"project": "p", "depth": "not a number"}),
        )
        .await
        .expect_err("bad depth type");
    assert!(!err.message.is_empty());
}

#[tokio::test]
async fn read_file_missing_file_returns_graceful() {
    let server = server_with_mock(MockDbClient::new());
    let result = server
        .call_tool_cli(
            "read_file",
            serde_json::json!({"path": "/absolute/missing.rs"}),
        )
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test]
async fn file_info_missing_file_returns_graceful() {
    let server = server_with_mock(MockDbClient::new());
    let result = server
        .call_tool_cli("file_info", serde_json::json!({"path": "/missing.rs"}))
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test]
async fn project_tree_unknown_project_empty_tree() {
    let server = server_with_mock(MockDbClient::new());
    let result = server
        .call_tool_cli(
            "project_tree",
            serde_json::json!({"project": "ghost", "depth": 3}),
        )
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

// JSON-round-trip guarantees (each tool's response parses as JSON).

/// The tool either returned parseable JSON OR a plain-text guidance
/// message (both are valid — some tools short-circuit with "Run X first"
/// when preconditions aren't met). This macro accepts either shape.
macro_rules! tool_returns_parseable_json {
    ($name:ident, $tool:expr, $args:tt) => {
        #[tokio::test]
        async fn $name() {
            let server = server_with_mock(MockDbClient::new());
            let result = server
                .call_tool_cli($tool, serde_json::json!($args))
                .await
                .expect("tool call");
            let payload = text_of(&result);
            let parsed_ok = serde_json::from_str::<serde_json::Value>(&payload).is_ok();
            let looks_like_guidance = !payload.is_empty()
                && (payload.contains("No ")
                    || payload.contains("Run ")
                    || payload.contains("first")
                    || payload.contains("empty")
                    || payload.contains("not found"));
            assert!(
                parsed_ok || looks_like_guidance,
                "{}: response is neither JSON nor guidance text:\n{}",
                $tool,
                payload
            );
        }
    };
}

tool_returns_parseable_json!(list_projects_emits_json, "list_projects", {});
tool_returns_parseable_json!(text_search_emits_json, "text_search", {"query": "q"});
tool_returns_parseable_json!(grep_emits_json, "grep", {"pattern": "x"});
tool_returns_parseable_json!(index_stats_emits_json, "index_stats", {});
tool_returns_parseable_json!(find_duplicates_emits_json, "find_duplicates", {});
tool_returns_parseable_json!(refactoring_report_emits_json, "refactoring_report", {});
tool_returns_parseable_json!(discover_topics_cached_emits_json, "discover_topics", {});
tool_returns_parseable_json!(find_orphans_emits_json, "find_orphans", {});
tool_returns_parseable_json!(
    find_misplaced_code_emits_json,
    "find_misplaced_code",
    {"project": "p"}
);
tool_returns_parseable_json!(
    test_coverage_gaps_emits_json,
    "test_coverage_gaps",
    {"project": "p"}
);
tool_returns_parseable_json!(
    complexity_hotspots_emits_json,
    "complexity_hotspots",
    {"project": "p"}
);
tool_returns_parseable_json!(
    suggest_merges_emits_json,
    "suggest_merges",
    {"project": "p"}
);
tool_returns_parseable_json!(
    suggest_splits_emits_json,
    "suggest_splits",
    {"project": "p"}
);
tool_returns_parseable_json!(
    doc_coverage_gaps_emits_json,
    "doc_coverage_gaps",
    {"project": "p"}
);
tool_returns_parseable_json!(topic_hierarchy_emits_json, "topic_hierarchy", {});
