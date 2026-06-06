//! SOTA Phases 7-11 integration tests.

use pgmcp_testing::pool_tool_helpers::{seed_file, seed_project, server_with_pool};
use pgmcp_testing::require_test_db;
use serde_json::Value;

fn text_of(result: &rmcp::model::CallToolResult) -> &str {
    for content in &result.content {
        if let rmcp::model::RawContent::Text(text) = &content.raw {
            return &text.text;
        }
    }
    panic!("tool returned no text content");
}

async fn seed_commit_chunk(pool: &sqlx::PgPool, project_id: i32, hash: &str, content: &str) {
    let commit_id: i64 = sqlx::query_scalar(
        "INSERT INTO git_commits (project_id, commit_hash, author, author_date, subject)
         VALUES ($1, $2, 'tester', now(), 'api stability test')
         RETURNING id",
    )
    .bind(project_id)
    .bind(hash)
    .fetch_one(pool)
    .await
    .expect("insert git commit");

    sqlx::query(
        "INSERT INTO git_commit_chunks (commit_id, chunk_index, content)
         VALUES ($1, 0, $2)",
    )
    .bind(commit_id)
    .bind(content)
    .execute(pool)
    .await
    .expect("insert git commit chunk");
}

// ============================================================================
// Phase 7 — API / contract
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
async fn public_api_surface_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p7-pas", "/ws/p7-pas").await;
    seed_file(db.pool(), p, "/ws/p7-pas/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "public_api_surface",
            serde_json::json!({"project": "p7-pas"}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn semver_break_audit_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p7-sba", "/ws/p7-sba").await;
    seed_file(db.pool(), p, "/ws/p7-sba/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "semver_break_audit",
            serde_json::json!({"project": "p7-sba"}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn deprecated_but_used_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p7-dbu", "/ws/p7-dbu").await;
    seed_file(db.pool(), p, "/ws/p7-dbu/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "deprecated_but_used",
            serde_json::json!({"project": "p7-dbu"}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn api_stability_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p7-as", "/ws/p7-as").await;
    seed_file(db.pool(), p, "/ws/p7-as/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli("api_stability", serde_json::json!({"project": "p7-as"}))
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn api_stability_bounds_and_scopes_commit_chunks() {
    let db = require_test_db!();
    let p1 = seed_project(db.pool(), "p7-as-scope", "/ws/p7-as-scope").await;
    let p2 = seed_project(db.pool(), "p7-as-other", "/ws/p7-as-other").await;
    seed_commit_chunk(
        db.pool(),
        p1,
        "api-stability-scope-1",
        "+ pub fn changed_api(input: i32) -> i32 { input }\n",
    )
    .await;
    seed_commit_chunk(
        db.pool(),
        p2,
        "api-stability-scope-2",
        "+ pub fn leaked_api(input: i32) -> i32 { input }\n",
    )
    .await;

    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "api_stability",
            serde_json::json!({
                "project": " p7-as-scope ",
                "window_commits": 0,
                "limit": -10,
            }),
        )
        .await
        .expect("api_stability");
    assert!(r.is_error != Some(true));
    let v: Value = serde_json::from_str(text_of(&r)).expect("api_stability JSON");

    assert_eq!(v["project"].as_str(), Some("p7-as-scope"));
    assert_eq!(v["window_commits"].as_i64(), Some(1));
    assert_eq!(v["limit"].as_i64(), Some(1));

    let symbols = v["symbols"].as_array().expect("symbols array");
    assert_eq!(symbols.len(), 1);
    assert_eq!(symbols[0]["name"].as_str(), Some("changed_api"));
    assert!(
        !symbols
            .iter()
            .any(|sym| sym["name"].as_str() == Some("leaked_api")),
        "api_stability leaked a commit chunk from another project: {v}"
    );
}

// ============================================================================
// Phase 8 — ML / embedding-based
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
async fn lsh_clone_detection_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p8-lsh", "/ws/p8-lsh").await;
    seed_file(db.pool(), p, "/ws/p8-lsh/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "lsh_clone_detection",
            serde_json::json!({"project": "p8-lsh"}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn semantic_drift_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p8-sd", "/ws/p8-sd").await;
    seed_file(db.pool(), p, "/ws/p8-sd/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli("semantic_drift", serde_json::json!({"project": "p8-sd"}))
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn embedding_outliers_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p8-eo", "/ws/p8-eo").await;
    seed_file(db.pool(), p, "/ws/p8-eo/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "embedding_outliers",
            serde_json::json!({"project": "p8-eo"}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn multi_resolution_pagerank_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p8-mrp", "/ws/p8-mrp").await;
    seed_file(db.pool(), p, "/ws/p8-mrp/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "multi_resolution_pagerank",
            serde_json::json!({"project": "p8-mrp"}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

// ============================================================================
// Phase 9 — Data engineering
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
async fn migration_safety_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p9-ms", "/ws/p9-ms").await;
    seed_file(db.pool(), p, "/ws/p9-ms/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli("migration_safety", serde_json::json!({"project": "p9-ms"}))
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn dead_columns_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p9-dc", "/ws/p9-dc").await;
    seed_file(db.pool(), p, "/ws/p9-dc/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli("dead_columns", serde_json::json!({"project": "p9-dc"}))
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn pii_spread_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p9-pii", "/ws/p9-pii").await;
    seed_file(db.pool(), p, "/ws/p9-pii/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli("pii_spread", serde_json::json!({"project": "p9-pii"}))
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

// ============================================================================
// Phase 10 — Call-graph downstream
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
async fn dead_code_reachability_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p10-dcr", "/ws/p10-dcr").await;
    seed_file(db.pool(), p, "/ws/p10-dcr/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "dead_code_reachability",
            serde_json::json!({"project": "p10-dcr"}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn feature_envy_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p10-fe", "/ws/p10-fe").await;
    seed_file(db.pool(), p, "/ws/p10-fe/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli("feature_envy", serde_json::json!({"project": "p10-fe"}))
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn shotgun_surgery_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p10-shot", "/ws/p10-shot").await;
    seed_file(db.pool(), p, "/ws/p10-shot/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "shotgun_surgery",
            serde_json::json!({"project": "p10-shot"}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn lcom4_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p10-lcom", "/ws/p10-lcom").await;
    seed_file(db.pool(), p, "/ws/p10-lcom/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli("lcom4", serde_json::json!({"project": "p10-lcom"}))
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

// ============================================================================
// Phase 11 — Evolution analytics
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
async fn refactor_pressure_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p11-rp", "/ws/p11-rp").await;
    seed_file(db.pool(), p, "/ws/p11-rp/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "refactor_pressure",
            serde_json::json!({"project": "p11-rp"}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn commit_changepoint_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p11-ccp", "/ws/p11-ccp").await;
    seed_file(db.pool(), p, "/ws/p11-ccp/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "commit_changepoint",
            serde_json::json!({"project": "p11-ccp"}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn commit_topic_drift_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p11-ctd", "/ws/p11-ctd").await;
    seed_file(db.pool(), p, "/ws/p11-ctd/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "commit_topic_drift",
            serde_json::json!({"project": "p11-ctd"}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn release_api_stability_runs() {
    let db = require_test_db!();
    let p = seed_project(db.pool(), "p11-ras", "/ws/p11-ras").await;
    seed_file(db.pool(), p, "/ws/p11-ras/a.rs", "a.rs").await;
    let server = server_with_pool(db.pool().clone());
    let r = server
        .call_tool_cli(
            "release_api_stability",
            serde_json::json!({"project": "p11-ras"}),
        )
        .await
        .expect("tool");
    assert!(r.is_error != Some(true));
}
