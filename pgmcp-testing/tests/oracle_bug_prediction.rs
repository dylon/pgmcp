//! Real-Postgres correctness oracle for `bug_prediction`.
//! Boost util/util.rs's fix_commit_ratio so its bug_score (which
//! weights fix_ratio at 3.0 — the largest coefficient) dominates.

use crate::common::{server_with_pool, text_of};
use pgmcp_testing::fixtures::synthetic_corpus::seed_graph_corpus;
use pgmcp_testing::require_test_db;
use uuid::Uuid;

#[tokio::test]
async fn bug_prediction_ranks_high_churn_high_fix_ratio_file_first() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let h = seed_graph_corpus(&pool).await;

    sqlx::query("UPDATE file_metrics SET fix_commit_ratio = 0.6 WHERE file_id = $1")
        .bind(h.files["util"].0)
        .execute(&pool)
        .await
        .expect("bump fix_ratio");

    let server = server_with_pool(pool);
    let result = server
        .call_tool_cli(
            "bug_prediction",
            serde_json::json!({"project": "graph-proj"}),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(v["limit"].as_u64(), Some(20));
    let files = v["files"].as_array().expect("files");
    assert!(!files.is_empty(), "bug_prediction returned no files");
    assert_eq!(
        files[0]["path"].as_str(),
        Some("util/util.rs"),
        "util/util.rs (high churn + high fix_ratio) must rank first; got {}",
        files[0]["path"]
    );
}

#[tokio::test]
async fn bug_prediction_clamps_negative_limit_to_one() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_graph_corpus(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "bug_prediction",
            serde_json::json!({"project": "graph-proj", "limit": -10}),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(v["limit"].as_u64(), Some(1));
    assert_eq!(v["file_count"].as_u64(), Some(1));
    assert_eq!(v["files"].as_array().expect("files").len(), 1);
}

#[tokio::test]
async fn bug_prediction_caps_large_limit_to_one_hundred() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_graph_corpus(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "bug_prediction",
            serde_json::json!({"project": "graph-proj", "limit": 500}),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(v["limit"].as_u64(), Some(100));
    assert_eq!(v["file_count"].as_u64(), Some(5));
}

#[tokio::test]
async fn bug_prediction_rejects_ambiguous_project_name() {
    let db = require_test_db!();
    let name = format!("duplicate-bug-prediction-{}", Uuid::now_v7().simple());
    for suffix in ["a", "b"] {
        sqlx::query("INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)")
            .bind(format!("/ws/{suffix}"))
            .bind(format!("/ws/{suffix}/{name}"))
            .bind(&name)
            .execute(db.pool())
            .await
            .expect("project");
    }

    let server = server_with_pool(db.pool().clone());
    let err = server
        .call_tool_cli("bug_prediction", serde_json::json!({"project": name}))
        .await
        .expect_err("duplicate project display names must fail closed");

    assert!(
        err.to_string().contains("ambiguous project name"),
        "unexpected bug_prediction ambiguity error: {err}"
    );
}
