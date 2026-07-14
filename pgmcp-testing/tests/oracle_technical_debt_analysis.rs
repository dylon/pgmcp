//! Real-Postgres correctness oracle for `technical_debt_analysis`.
//! Inject TODO/FIXME markers into util/util.rs's content; combined
//! with its already-high churn rate this produces the highest
//! debt_score.

use crate::common::{server_with_pool, text_of};
use pgmcp_testing::fixtures::synthetic_corpus::seed_graph_corpus;
use pgmcp_testing::require_test_db;
use uuid::Uuid;

#[tokio::test]
async fn technical_debt_analysis_ranks_high_churn_files_higher() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let h = seed_graph_corpus(&pool).await;

    sqlx::query("UPDATE indexed_files SET content = $1, line_count = 200 WHERE id = $2")
        .bind(
            "// TODO: refactor this\n\
             // FIXME: handle errors\n\
             // TODO: add tests\n\
             fn body() {}\n",
        )
        .bind(h.files["util"].0)
        .execute(&pool)
        .await
        .expect("inject todos");

    let server = server_with_pool(pool);
    let result = server
        .call_tool_cli(
            "technical_debt_analysis",
            serde_json::json!({"project": "graph-proj", "include_todos": true}),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(v["limit"].as_u64(), Some(30));
    let files = v["files"].as_array().expect("files");
    assert!(!files.is_empty(), "no files in debt analysis");
    assert_eq!(
        files[0]["path"].as_str(),
        Some("util/util.rs"),
        "util (TODO-laden + high churn) must top the debt ranking; got {}",
        files[0]["path"]
    );
    let total_markers = v["total_debt_markers"]
        .as_i64()
        .expect("total_debt_markers");
    assert!(
        total_markers >= 3,
        "expected ≥ 3 debt markers across the project; got {total_markers}"
    );
}

#[tokio::test]
async fn technical_debt_analysis_clamps_negative_limit_to_one() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_graph_corpus(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "technical_debt_analysis",
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
async fn technical_debt_analysis_caps_large_limit_to_one_hundred() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_graph_corpus(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "technical_debt_analysis",
            serde_json::json!({"project": "graph-proj", "limit": 500}),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(v["limit"].as_u64(), Some(100));
    assert_eq!(v["file_count"].as_u64(), Some(5));
}

#[tokio::test]
async fn technical_debt_analysis_rejects_ambiguous_project_name() {
    let db = require_test_db!();
    let name = format!("duplicate-technical-debt-{}", Uuid::now_v7().simple());
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
        .call_tool_cli(
            "technical_debt_analysis",
            serde_json::json!({"project": name}),
        )
        .await
        .expect_err("duplicate project display names must fail closed");

    assert!(
        err.to_string().contains("ambiguous project name"),
        "unexpected technical_debt_analysis ambiguity error: {err}"
    );
}
