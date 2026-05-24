//! P13.4 — tool_phonetic_grep_comments uses real PhoneticGrepOnline.
//!
//! The old stub scored each haystack line by raw articulatory
//! distance (returning a per-line list, not character-level matches).
//! The real implementation runs `PhoneticGrepOnline::scan` per line
//! and returns character-anchored matches with normalized text,
//! byte ranges, and distances.
//!
//! This test:
//!   1. Confirms a non-stub response shape (matches contain
//!      `byte_start`, `byte_end`, `original_text`,
//!      `normalized_text`).
//!   2. Confirms phonetic matching: "fone" matches "phone" via the
//!      embedded English ph→f rule.

use std::sync::Arc;

use arc_swap::ArcSwap;
use pgmcp::config::Config;
use pgmcp::context::SystemContext;
use pgmcp::daemon_state::DaemonLifecycle;
use pgmcp::db::DbClient;
use pgmcp::embed::EmbedSource;
use pgmcp::mcp::logging::LogBroadcaster;
use pgmcp::mcp::server::PhoneticGrepCommentsParams;
use pgmcp::mcp::tasks::TaskStore;
use pgmcp::mcp::tools::tool_phonetic_grep_comments;
use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::mocks::DeterministicEmbeddingBackend;
use pgmcp_testing::require_test_db;

fn build_ctx(db: Arc<dyn DbClient>) -> SystemContext {
    let config = Arc::new(ArcSwap::from_pointee(Config::default()));
    let stats = Arc::new(StatsTracker::new());
    let log_broadcaster = Arc::new(LogBroadcaster::new());
    let task_store = Arc::new(TaskStore::new());
    let embed_backend: Arc<dyn pgmcp::embed::EmbeddingBackend> =
        Arc::new(DeterministicEmbeddingBackend::new(384));
    let embed_source = EmbedSource::backend(embed_backend);
    let lifecycle = DaemonLifecycle::new();
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

#[tokio::test(flavor = "multi_thread")]
async fn phonetic_grep_finds_ph_to_f_via_english_rules() {
    let testdb = require_test_db!();
    let db: Arc<dyn DbClient> = Arc::new(testdb.pool().clone());
    let ctx = build_ctx(db);

    let params = PhoneticGrepCommentsParams {
        query: "fone".to_string(),
        haystack: vec![
            "call the phone".to_string(),
            "completely unrelated text".to_string(),
            "another line referencing a phonE call".to_string(),
        ],
    };
    let result = tool_phonetic_grep_comments::run(&ctx, params)
        .await
        .expect("tool call");
    let text = result
        .content
        .iter()
        .find_map(|c| c.as_text().map(|t| t.text.clone()))
        .expect("text content");
    let val: serde_json::Value = serde_json::from_str(&text).expect("json");

    // Real implementation must surface match-level (not line-level)
    // results with the framework's positional fields.
    let matches = val
        .get("matches")
        .and_then(|m| m.as_array())
        .expect("matches array");
    assert!(
        !matches.is_empty(),
        "fone vs `phone` lines must produce at least one phonetic match; got {val:#}"
    );
    let first = &matches[0];
    for field in ["byte_start", "byte_end", "original_text", "normalized_text"] {
        assert!(
            first.get(field).is_some(),
            "match record must include {field}; got {first:#}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_haystack_returns_zero_matches() {
    let testdb = require_test_db!();
    let db: Arc<dyn DbClient> = Arc::new(testdb.pool().clone());
    let ctx = build_ctx(db);
    let params = PhoneticGrepCommentsParams {
        query: "anything".to_string(),
        haystack: vec![],
    };
    let result = tool_phonetic_grep_comments::run(&ctx, params)
        .await
        .expect("call");
    let text = result
        .content
        .iter()
        .find_map(|c| c.as_text().map(|t| t.text.clone()))
        .expect("text");
    let val: serde_json::Value = serde_json::from_str(&text).expect("json");
    assert_eq!(val["match_count"], 0);
}
