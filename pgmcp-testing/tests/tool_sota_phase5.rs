//! SOTA Phase 5 (concurrency / safety / performance) integration tests.

use pgmcp_testing::pool_tool_helpers::{seed_file, seed_project, server_with_pool};
use pgmcp_testing::require_test_db;

#[tokio::test(flavor = "multi_thread")]
async fn lockset_races_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p5-lockset", "/ws/p5-lockset").await;
    seed_file(db.pool(), p, "/ws/p5-lockset/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "lockset_races",
            serde_json::json!({"project": "p5-lockset"}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn unsafe_clusters_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p5-uc", "/ws/p5-uc").await;
    seed_file(db.pool(), p, "/ws/p5-uc/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli("unsafe_clusters", serde_json::json!({"project": "p5-uc"}))
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn panic_paths_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p5-pp", "/ws/p5-pp").await;
    seed_file(db.pool(), p, "/ws/p5-pp/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli("panic_paths", serde_json::json!({"project": "p5-pp"}))
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn deadlock_candidates_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p5-dl", "/ws/p5-dl").await;
    seed_file(db.pool(), p, "/ws/p5-dl/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "deadlock_candidates",
            serde_json::json!({"project": "p5-dl"}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn send_sync_violations_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p5-ss", "/ws/p5-ss").await;
    seed_file(db.pool(), p, "/ws/p5-ss/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "send_sync_violations",
            serde_json::json!({"project": "p5-ss"}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn quadratic_loops_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p5-ql", "/ws/p5-ql").await;
    seed_file(db.pool(), p, "/ws/p5-ql/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli("quadratic_loops", serde_json::json!({"project": "p5-ql"}))
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_preallocation_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p5-mp", "/ws/p5-mp").await;
    seed_file(db.pool(), p, "/ws/p5-mp/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "missing_preallocation",
            serde_json::json!({"project": "p5-mp"}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn blocking_in_async_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p5-ba", "/ws/p5-ba").await;
    seed_file(db.pool(), p, "/ws/p5-ba/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli("blocking_in_async", serde_json::json!({"project": "p5-ba"}))
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn clone_density_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p5-cd", "/ws/p5-cd").await;
    seed_file(db.pool(), p, "/ws/p5-cd/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli("clone_density", serde_json::json!({"project": "p5-cd"}))
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn io_hotpath_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p5-ih", "/ws/p5-ih").await;
    seed_file(db.pool(), p, "/ws/p5-ih/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli("io_hotpath", serde_json::json!({"project": "p5-ih"}))
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}
