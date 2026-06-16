//! Verifies the `topics-size-history` cron is wired into the scheduler and
//! declared as a module, and that one run snapshots into
//! `pgmcp_metadata['topics_size_history']`. Mirrors the source-introspection
//! half of `quality_history_cron_registered.rs`.

use std::path::{Path, PathBuf};

use pgmcp_testing::fixtures::synthetic_corpus::SyntheticCorpus;
use pgmcp_testing::require_test_db;

fn repo_root() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    Path::new(&manifest)
        .parent()
        .expect("workspace root above pgmcp-testing")
        .to_path_buf()
}

#[test]
fn scheduler_source_registers_topics_size_history() {
    let src = std::fs::read_to_string(repo_root().join("src/cron/scheduler.rs"))
        .expect("read scheduler.rs");
    assert!(
        src.contains("\"topics-size-history\""),
        "scheduler must register the topics-size-history schedule name"
    );
    assert!(
        src.contains("topics_size_history::run_or_log"),
        "scheduler must dispatch into topics_size_history::run_or_log"
    );
}

#[test]
fn cron_mod_declares_topics_size_history() {
    let src =
        std::fs::read_to_string(repo_root().join("src/cron/mod.rs")).expect("read cron/mod.rs");
    assert!(
        src.contains("pub mod topics_size_history;"),
        "cron/mod.rs must declare the topics_size_history module"
    );
}

#[tokio::test]
async fn topics_size_history_snapshots_into_metadata() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    SyntheticCorpus::seed_with_assignments(&pool).await;

    pgmcp::cron::topics_size_history::run_or_log(&pool).await;

    let raw: Option<String> =
        sqlx::query_scalar("SELECT value FROM pgmcp_metadata WHERE key = 'topics_size_history'")
            .fetch_optional(&pool)
            .await
            .expect("query history");
    let arr: Vec<serde_json::Value> =
        serde_json::from_str(&raw.expect("history written")).expect("parse history");
    assert_eq!(arr.len(), 1, "one snapshot after one run");
    let topics = arr[0]["topics"].as_array().expect("topics array");
    assert!(
        topics.len() >= 3,
        "the 3 seeded global topics must be snapshotted, got {}",
        topics.len()
    );
}
