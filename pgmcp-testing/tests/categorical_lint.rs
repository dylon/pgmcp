//! Integration test for `categorical_lint` (ADR-028, item 4): the strict
//! extensive-sum law (workspace file_count == Σ project file_count) passes when
//! the rollup is consistent and is flagged when it is not. Drives the dispatched
//! tool via `call_tool_cli` (Layer-D coverage gate).

mod common;

use common::{server_with_pool, text_of};
use pgmcp_testing::require_test_db;
use serde_json::json;

fn body(r: &rmcp::model::CallToolResult) -> serde_json::Value {
    serde_json::from_str(&text_of(r)).expect("tool body must be JSON")
}

#[tokio::test]
async fn categorical_lint_checks_extensive_sum_law() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let server = server_with_pool(pool.clone());

    for (name, fc) in [("cl_a", 10_i32), ("cl_b", 20_i32)] {
        let pid: i32 = sqlx::query_scalar(
            "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $1, $2)
             ON CONFLICT (path) DO UPDATE SET name = $2 RETURNING id",
        )
        .bind(format!("/ws/{name}"))
        .bind(name)
        .fetch_one(&pool)
        .await
        .expect("project");
        sqlx::query(
            "INSERT INTO project_metrics (project_id, file_count) VALUES ($1, $2)
             ON CONFLICT (project_id) DO UPDATE SET file_count = $2",
        )
        .bind(pid)
        .bind(fc)
        .execute(&pool)
        .await
        .expect("project_metrics");
    }

    // rebuild=true → workspace file_count = 30 = Σ → law holds.
    let ok = body(
        &server
            .call_tool_cli("categorical_lint", json!({"rebuild": true}))
            .await
            .expect("categorical_lint"),
    );
    assert_eq!(ok["ok"], true, "consistent rollup must pass: {ok}");
    assert_eq!(ok["violations"].as_array().unwrap().len(), 0);

    // Corrupt the workspace total → the strict law must flag it.
    sqlx::query("UPDATE hier_group_metrics SET file_count = 999 WHERE level = 'workspace'")
        .execute(&pool)
        .await
        .expect("corrupt workspace row");
    let bad = body(
        &server
            .call_tool_cli("categorical_lint", json!({"rebuild": false}))
            .await
            .expect("categorical_lint"),
    );
    assert_eq!(bad["ok"], false, "inconsistent rollup must fail: {bad}");
    let v = bad["violations"].as_array().unwrap();
    assert!(
        v.iter().any(|x| x["law"] == "file_count_extensive"),
        "the file_count law must be the violation: {bad}"
    );
}
