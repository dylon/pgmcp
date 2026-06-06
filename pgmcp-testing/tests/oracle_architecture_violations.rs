//! Real-Postgres correctness oracle for `architecture_violations`.

mod common;

use common::{server_with_pool, text_of};
use pgmcp_testing::fixtures::synthetic_corpus::seed_graph_corpus;
use pgmcp_testing::require_test_db;
use serde_json::Value;
use sqlx::PgPool;

async fn insert_project(pool: &PgPool, name: &str, workspace: &str) -> i32 {
    sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(workspace)
    .bind(format!("{workspace}/{name}"))
    .bind(name)
    .fetch_one(pool)
    .await
    .expect("insert project")
}

async fn insert_file(pool: &PgPool, project_id: i32, path: &str, relative_path: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO indexed_files
         (project_id, path, relative_path, language, size_bytes, line_count, modified_at)
         VALUES ($1, $2, $3, 'rust', 10, 10, NOW())
         RETURNING id",
    )
    .bind(project_id)
    .bind(path)
    .bind(relative_path)
    .fetch_one(pool)
    .await
    .expect("insert file")
}

async fn insert_import_edge(pool: &PgPool, project_id: i32, source: i64, target: i64) {
    sqlx::query(
        "INSERT INTO code_graph_edges
         (project_id, source_file_id, target_file_id, edge_type, weight)
         VALUES ($1, $2, $3, 'import', 1.0)",
    )
    .bind(project_id)
    .bind(source)
    .bind(target)
    .execute(pool)
    .await
    .expect("insert import edge");
}

fn violation_count(payload: &serde_json::Value) -> u64 {
    if let Some(c) = payload["violation_count"].as_u64() {
        return c;
    }
    if let Some(arr) = payload["violations"].as_array() {
        return arr.len() as u64;
    }
    panic!("neither `violation_count` nor `violations` field present in payload: {payload}");
}

#[tokio::test]
async fn architecture_violations_empty_project_returns_zero_violations() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    insert_project(&pool, "arch-empty", "/ws/arch-empty").await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "architecture_violations",
            serde_json::json!({
                "project": " arch-empty ",
                "severity_threshold": " low ",
                "include_fixes": false
            }),
        )
        .await
        .expect("call");
    let v: Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(v["project"], "arch-empty");
    assert_eq!(v["severity_threshold"], "low");
    let count = violation_count(&v);
    assert_eq!(
        count, 0,
        "empty project must produce 0 violations; got {count}\npayload: {v}"
    );
    assert_eq!(v["total_violation_count"], 0);
    assert_eq!(v["truncated"], false);
}

#[tokio::test]
async fn architecture_violations_detects_dependency_cycles() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_graph_corpus(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "architecture_violations",
            serde_json::json!({
                "project": "graph-proj",
                "severity_threshold": "critical",
                "include_fixes": false
            }),
        )
        .await
        .expect("call");
    let v: Value = serde_json::from_str(&text_of(&result)).expect("json");
    let count = violation_count(&v);
    assert!(
        count >= 1,
        "planted import cycles must produce critical violations; got {count}\npayload: {v}"
    );
    let violations = v["violations"].as_array().expect("violations");
    assert!(
        violations
            .iter()
            .all(|violation| violation["severity"] == "critical"),
        "critical threshold must suppress non-critical violations: {v}"
    );
    assert!(
        violations
            .iter()
            .any(|violation| violation["type"] == "dependency_cycle"),
        "expected at least one dependency_cycle violation: {v}"
    );
}

#[tokio::test]
async fn architecture_violations_rejects_invalid_severity_and_duplicate_projects() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    sqlx::query("INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)")
        .bind("/ws/arch-dup-a")
        .bind("/ws/arch-dup-a/arch-dup")
        .bind("arch-dup")
        .execute(&pool)
        .await
        .expect("insert first duplicate project");
    sqlx::query("INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)")
        .bind("/ws/arch-dup-b")
        .bind("/ws/arch-dup-b/arch-dup")
        .bind("arch-dup")
        .execute(&pool)
        .await
        .expect("insert second duplicate project");
    let server = server_with_pool(pool);

    let invalid = server
        .call_tool_cli(
            "architecture_violations",
            serde_json::json!({"project": "arch-dup", "severity_threshold": "severe"}),
        )
        .await
        .expect_err("invalid severity must fail closed before scanning");
    assert!(
        invalid.to_string().contains("Unknown severity_threshold"),
        "unexpected invalid severity error: {invalid}"
    );

    let duplicate = server
        .call_tool_cli(
            "architecture_violations",
            serde_json::json!({"project": "arch-dup", "severity_threshold": "low"}),
        )
        .await
        .expect_err("duplicate project display names must fail closed");
    assert!(
        duplicate.to_string().contains("ambiguous project name"),
        "unexpected duplicate project error: {duplicate}"
    );
}

#[tokio::test]
async fn architecture_violations_ignores_cross_project_edges() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let project_id = insert_project(&pool, "arch-scope", "/ws/arch-scope").await;
    let other_project_id = insert_project(&pool, "arch-other", "/ws/arch-other").await;
    let source = insert_file(&pool, project_id, "/ws/arch-scope/src/a.rs", "src/a.rs").await;
    let foreign = insert_file(
        &pool,
        other_project_id,
        "/ws/arch-other/src/b.rs",
        "src/b.rs",
    )
    .await;

    // Both rows carry project_id = arch-scope, but at least one endpoint belongs
    // to arch-other. The tool must reject both at the SQL boundary.
    insert_import_edge(&pool, project_id, source, foreign).await;
    insert_import_edge(&pool, project_id, foreign, source).await;

    let server = server_with_pool(pool);
    let result = server
        .call_tool_cli(
            "architecture_violations",
            serde_json::json!({
                "project": "arch-scope",
                "severity_threshold": "low",
                "include_fixes": false
            }),
        )
        .await
        .expect("architecture_violations call");
    let v: Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(
        violation_count(&v),
        0,
        "cross-project edges must not create cycles or bidirectional violations: {v}"
    );
}
