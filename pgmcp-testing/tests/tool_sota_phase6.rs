//! SOTA Phase 6 (security) integration tests.

use pgmcp_testing::pool_tool_helpers::{seed_file, seed_project, server_with_pool};
use pgmcp_testing::require_test_db;

#[tokio::test(flavor = "multi_thread")]
async fn taint_analysis_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p6-ta", "/ws/p6-ta").await;
    seed_file(db.pool(), p, "/ws/p6-ta/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli("taint_analysis", serde_json::json!({"project": "p6-ta"}))
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn secret_detection_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p6-sd", "/ws/p6-sd").await;
    seed_file(db.pool(), p, "/ws/p6-sd/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli("secret_detection", serde_json::json!({"project": "p6-sd"}))
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn crypto_misuse_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p6-cm", "/ws/p6-cm").await;
    seed_file(db.pool(), p, "/ws/p6-cm/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli("crypto_misuse", serde_json::json!({"project": "p6-cm"}))
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn unsafe_deserialization_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p6-ud", "/ws/p6-ud").await;
    seed_file(db.pool(), p, "/ws/p6-ud/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "unsafe_deserialization",
            serde_json::json!({"project": "p6-ud"}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn injection_candidates_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p6-ic", "/ws/p6-ic").await;
    seed_file(db.pool(), p, "/ws/p6-ic/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "injection_candidates",
            serde_json::json!({"project": "p6-ic"}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn unprotected_routes_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p6-ur", "/ws/p6-ur").await;
    seed_file(db.pool(), p, "/ws/p6-ur/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "unprotected_routes",
            serde_json::json!({"project": "p6-ur"}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn cve_supply_chain_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p6-cve", "/ws/p6-cve").await;
    seed_file(db.pool(), p, "/ws/p6-cve/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli("cve_supply_chain", serde_json::json!({"project": "p6-cve"}))
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}
