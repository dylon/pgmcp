//! Real-Postgres correctness oracle for `design_metrics`.
//! The synthetic graph has five files with import-degree metrics and content,
//! enough to pin per-file metrics, module-prefix scoping, limit normalization,
//! and duplicate project-name rejection.

use crate::common::{server_with_pool, text_of};
use pgmcp_testing::fixtures::synthetic_corpus::seed_graph_corpus;
use pgmcp_testing::require_test_db;
use uuid::Uuid;

#[tokio::test]
async fn design_metrics_reports_per_file_metrics() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_graph_corpus(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "design_metrics",
            serde_json::json!({"project": "graph-proj"}),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(v["scope"], "project");
    assert_eq!(v["limit"].as_u64(), Some(30));
    assert_eq!(v["file_count"].as_u64(), Some(5));

    let files = v["files"].as_array().expect("files");
    assert_eq!(files.len(), 5);
    for file in files {
        assert!(file.get("path").is_some());
        assert!(file.get("cyclomatic_complexity").is_some());
        assert!(file.get("wmc").is_some());
        assert!(file.get("structural_complexity").is_some());
        assert!(file.get("data_complexity").is_some());
        assert!(file.get("system_complexity").is_some());
        assert!(file.get("maintainability_index").is_some());
        assert!(file.get("fan_in").is_some());
        assert!(file.get("fan_out").is_some());
    }
}

#[tokio::test]
async fn design_metrics_clamps_negative_limit_to_one() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_graph_corpus(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "design_metrics",
            serde_json::json!({"project": "graph-proj", "limit": -5}),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(v["limit"].as_u64(), Some(1));
    assert_eq!(v["file_count"].as_u64(), Some(1));
    assert_eq!(v["files"].as_array().expect("files").len(), 1);
}

#[tokio::test]
async fn design_metrics_caps_large_limit_to_one_hundred() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_graph_corpus(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "design_metrics",
            serde_json::json!({"project": "graph-proj", "limit": 500}),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(v["limit"].as_u64(), Some(100));
    assert_eq!(v["file_count"].as_u64(), Some(5));
}

#[tokio::test]
async fn design_metrics_module_scope_uses_literal_prefix() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_graph_corpus(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "design_metrics",
            serde_json::json!({"project": "graph-proj", "scope": "module", "path": "core/"}),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(v["scope"], "module");
    let files = v["files"].as_array().expect("files");
    assert_eq!(files.len(), 3);
    for file in files {
        let path = file["path"].as_str().expect("path");
        assert!(
            path.starts_with("core/"),
            "module scope leaked non-core path: {path}"
        );
    }
}

#[tokio::test]
async fn design_metrics_rejects_ambiguous_project_name() {
    let db = require_test_db!();
    let name = format!("duplicate-design-metrics-{}", Uuid::now_v7().simple());
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
        .call_tool_cli("design_metrics", serde_json::json!({"project": name}))
        .await
        .expect_err("duplicate project display names must fail closed");

    assert!(
        err.to_string().contains("ambiguous project name"),
        "unexpected design_metrics ambiguity error: {err}"
    );
}
