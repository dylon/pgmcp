//! Phase 6: real-DB integration tests for the Topic tool category.
//! Exercises the pool()-using branches of `discover_topics` (realtime
//! FCM) and `topic_hierarchy`.

use pgmcp_testing::pool_tool_helpers::{seed_file, seed_project, server_with_pool};
use pgmcp_testing::require_test_db;

#[tokio::test(flavor = "multi_thread")]
async fn discover_topics_realtime_branch_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "dtopic-p", "/ws/dtopic-p").await;
    seed_file(db.pool(), p, "/ws/dtopic-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    // Project arg triggers the realtime FCM branch (pool()-using).
    let result = server
        .call_tool_cli(
            "discover_topics",
            serde_json::json!({"project": "dtopic-p"}),
        )
        .await
        .expect("tool call");
    let _ = result;
}

#[tokio::test(flavor = "multi_thread")]
async fn topic_hierarchy_runs_against_real_db() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "th-p", "/ws/th-p").await;
    seed_file(db.pool(), p, "/ws/th-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let result = server
        .call_tool_cli("topic_hierarchy", serde_json::json!({"project": "th-p"}))
        .await
        .expect("tool call");
    let _ = result;
}
