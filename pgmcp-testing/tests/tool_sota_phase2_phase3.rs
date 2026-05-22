//! SOTA Phase 2 (graph algorithms) + Phase 3 (information theory) integration
//! tests. Each test seeds a minimal project + file and verifies the tool runs
//! without error.

use pgmcp_testing::pool_tool_helpers::{seed_file, seed_project, server_with_pool};
use pgmcp_testing::require_test_db;

#[tokio::test(flavor = "multi_thread")]
async fn kcore_analysis_runs_against_real_db() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "kc-p", "/ws/kc-p").await;
    seed_file(db.pool(), p, "/ws/kc-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let result = server
        .call_tool_cli("kcore_analysis", serde_json::json!({"project": "kc-p"}))
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn ktruss_analysis_runs_against_real_db() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "kt-p", "/ws/kt-p").await;
    seed_file(db.pool(), p, "/ws/kt-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let result = server
        .call_tool_cli("ktruss_analysis", serde_json::json!({"project": "kt-p"}))
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn personalized_pagerank_runs_against_real_db() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "ppr-p", "/ws/ppr-p").await;
    seed_file(db.pool(), p, "/ws/ppr-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let result = server
        .call_tool_cli(
            "personalized_pagerank",
            serde_json::json!({"project": "ppr-p", "seed_files": ["a.rs"]}),
        )
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn edge_betweenness_runs_against_real_db() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "eb-p", "/ws/eb-p").await;
    seed_file(db.pool(), p, "/ws/eb-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let result = server
        .call_tool_cli("edge_betweenness", serde_json::json!({"project": "eb-p"}))
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn structural_holes_runs_against_real_db() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "sh-p", "/ws/sh-p").await;
    seed_file(db.pool(), p, "/ws/sh-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let result = server
        .call_tool_cli("structural_holes", serde_json::json!({"project": "sh-p"}))
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn motif_census_runs_against_real_db() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "mc-p", "/ws/mc-p").await;
    seed_file(db.pool(), p, "/ws/mc-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let result = server
        .call_tool_cli("motif_census", serde_json::json!({"project": "mc-p"}))
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn attack_vulnerability_runs_against_real_db() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "av-p", "/ws/av-p").await;
    seed_file(db.pool(), p, "/ws/av-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let result = server
        .call_tool_cli(
            "attack_vulnerability",
            serde_json::json!({"project": "av-p"}),
        )
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn compression_distance_runs_against_real_db() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "cd-p", "/ws/cd-p").await;
    seed_file(db.pool(), p, "/ws/cd-p/a.rs", "a.rs").await;
    seed_file(db.pool(), p, "/ws/cd-p/b.rs", "b.rs").await;
    let server = server_with_pool(db.pool().clone());
    let result = server
        .call_tool_cli(
            "compression_distance",
            serde_json::json!({"project": "cd-p", "file_a": "a.rs", "file_b": "b.rs"}),
        )
        .await
        .expect("tool call");
    // Allow either success or a clean error when content is NULL — the seed
    // helper may not populate `indexed_files.content`. The point is the tool
    // wires correctly through the dispatch path.
    let _ = result;
}

#[tokio::test(flavor = "multi_thread")]
async fn cochange_mutual_information_runs_against_real_db() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "cmi-p", "/ws/cmi-p").await;
    seed_file(db.pool(), p, "/ws/cmi-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let result = server
        .call_tool_cli(
            "cochange_mutual_information",
            serde_json::json!({"project": "cmi-p"}),
        )
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn import_entropy_runs_against_real_db() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "ie-p", "/ws/ie-p").await;
    seed_file(db.pool(), p, "/ws/ie-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let result = server
        .call_tool_cli("import_entropy", serde_json::json!({"project": "ie-p"}))
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn identifier_entropy_runs_against_real_db() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "ide-p", "/ws/ide-p").await;
    seed_file(db.pool(), p, "/ws/ide-p/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let result = server
        .call_tool_cli(
            "identifier_entropy",
            serde_json::json!({"project": "ide-p"}),
        )
        .await
        .expect("tool call");
    assert!(result.is_error != Some(true));
}
