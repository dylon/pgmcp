//! SOTA Phase 4 (evolution + quality) integration tests.

use pgmcp_testing::pool_tool_helpers::{seed_file, seed_project, server_with_pool};
use pgmcp_testing::require_test_db;

#[tokio::test(flavor = "multi_thread")]
async fn bus_factor_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "bf-p", "/ws/bf-p").await;
    seed_file(db.pool(), p, "/ws/bf-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli("bus_factor", serde_json::json!({"project": "bf-p"}))
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn knowledge_silos_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "ks-p", "/ws/ks-p").await;
    seed_file(db.pool(), p, "/ws/ks-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli("knowledge_silos", serde_json::json!({"project": "ks-p"}))
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn ownership_coupling_mismatch_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "ocm-p", "/ws/ocm-p").await;
    seed_file(db.pool(), p, "/ws/ocm-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "ownership_coupling_mismatch",
            serde_json::json!({"project": "ocm-p"}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn doc_code_drift_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "dcd-p", "/ws/dcd-p").await;
    seed_file(db.pool(), p, "/ws/dcd-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli("doc_code_drift", serde_json::json!({"project": "dcd-p"}))
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_smells_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "ts-p", "/ws/ts-p").await;
    seed_file(db.pool(), p, "/ws/ts-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli("test_smells", serde_json::json!({"project": "ts-p"}))
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn mutation_score_surrogate_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "mss-p", "/ws/mss-p").await;
    seed_file(db.pool(), p, "/ws/mss-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "mutation_score_surrogate",
            serde_json::json!({"project": "mss-p"}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn flaky_test_candidates_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "ftc-p", "/ws/ftc-p").await;
    seed_file(db.pool(), p, "/ws/ftc-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "flaky_test_candidates",
            serde_json::json!({"project": "ftc-p"}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}
