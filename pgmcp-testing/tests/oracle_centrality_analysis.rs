//! Real-Postgres correctness oracle for `centrality_analysis`. The
//! synthetic graph corpus pre-loads `file_metrics` rows with known
//! pagerank scores so the rank order is deterministic.

mod common;

use common::{server_with_pool, text_of};
use pgmcp_testing::fixtures::synthetic_corpus::seed_graph_corpus;
use pgmcp_testing::require_test_db;
use uuid::Uuid;

#[tokio::test]
async fn centrality_analysis_pagerank_orders_files_by_pinned_metrics() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_graph_corpus(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "centrality_analysis",
            serde_json::json!({"project": " graph-proj ", "metric": " pagerank "}),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(v["project"].as_str(), Some("graph-proj"));
    assert_eq!(v["metric"].as_str(), Some("pagerank"));
    assert_eq!(v["limit"].as_u64(), Some(20));
    let files = v["files"].as_array().expect("files");
    assert_eq!(files.len(), 5);
    // Pinned metrics: a=0.30, b=0.20, util=0.20, c=0.15, api=0.15
    // — top file must be core/a.rs.
    assert_eq!(files[0]["path"].as_str(), Some("core/a.rs"));
    let top_pr: f64 = files[0]["pagerank"]
        .as_str()
        .unwrap()
        .parse()
        .expect("parse");
    assert!(
        (top_pr - 0.30).abs() < 1e-3,
        "top pagerank = {top_pr}, expected 0.30"
    );
}

#[tokio::test]
async fn centrality_analysis_clamps_negative_limit_to_one() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_graph_corpus(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "centrality_analysis",
            serde_json::json!({"project": "graph-proj", "metric": "degree", "limit": -10}),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(v["limit"].as_u64(), Some(1));
    assert_eq!(v["file_count"].as_u64(), Some(1));
    assert_eq!(v["files"].as_array().expect("files").len(), 1);
}

#[tokio::test]
async fn centrality_analysis_caps_large_limit_to_two_hundred() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_graph_corpus(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "centrality_analysis",
            serde_json::json!({"project": "graph-proj", "limit": 500}),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(v["limit"].as_u64(), Some(200));
    assert_eq!(v["file_count"].as_u64(), Some(5));
}

#[tokio::test]
async fn centrality_analysis_rejects_unknown_metric() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_graph_corpus(&pool).await;
    let server = server_with_pool(pool);

    let err = server
        .call_tool_cli(
            "centrality_analysis",
            serde_json::json!({"project": "graph-proj", "metric": "eigenvector"}),
        )
        .await
        .expect_err("unknown centrality metric must fail closed");

    assert!(
        err.to_string().contains("unknown metric"),
        "unexpected centrality metric error: {err}"
    );
}

#[tokio::test]
async fn centrality_analysis_rejects_ambiguous_project_name() {
    let db = require_test_db!();
    let name = format!("duplicate-centrality-{}", Uuid::now_v7().simple());
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
        .call_tool_cli("centrality_analysis", serde_json::json!({"project": name}))
        .await
        .expect_err("duplicate project display names must fail closed");

    assert!(
        err.to_string().contains("ambiguous project name"),
        "unexpected centrality ambiguity error: {err}"
    );
}
