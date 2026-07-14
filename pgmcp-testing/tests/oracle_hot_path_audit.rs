//! Real-Postgres correctness oracle for `hot_path_audit`.

use crate::common::{server_with_pool, text_of};
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

async fn insert_metric(
    pool: &PgPool,
    metric_project_id: i32,
    file_id: i64,
    pagerank: f64,
    churn_rate: f64,
    fix_commit_ratio: f64,
) {
    sqlx::query(
        "INSERT INTO file_metrics
         (file_id, project_id, pagerank, churn_rate, fix_commit_ratio,
          bug_proneness, instability, in_degree, author_count, commit_count)
         VALUES ($1, $2, $3, $4, $5, 0.1, 0.1, 1, 1, 1)",
    )
    .bind(file_id)
    .bind(metric_project_id)
    .bind(pagerank)
    .bind(churn_rate)
    .bind(fix_commit_ratio)
    .execute(pool)
    .await
    .expect("insert metric");
}

fn hot_paths(payload: &Value) -> &[Value] {
    payload["hot_paths"].as_array().expect("hot_paths")
}

#[tokio::test]
async fn hot_path_audit_normalizes_and_bounds_request() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let project_id = insert_project(&pool, "hot-proj", "/ws/hot-proj").await;
    let low = insert_file(&pool, project_id, "/ws/hot-proj/src/low.rs", "src/low.rs").await;
    let high = insert_file(&pool, project_id, "/ws/hot-proj/src/high.rs", "src/high.rs").await;
    insert_metric(&pool, project_id, low, 0.1, 0.1, 0.1).await;
    insert_metric(&pool, project_id, high, 10.0, 10.0, 10.0).await;

    let server = server_with_pool(pool);
    let result = server
        .call_tool_cli(
            "hot_path_audit",
            serde_json::json!({
                "project": " hot-proj ",
                "percentile_threshold": 2.0,
                "limit": -20
            }),
        )
        .await
        .expect("hot_path_audit call");
    let v: Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(v["parameters"]["project"], "hot-proj");
    assert_eq!(v["parameters"]["percentile_threshold"], 1.0);
    assert_eq!(v["parameters"]["limit"], 1);
    assert_eq!(hot_paths(&v).len(), 1);
    assert_eq!(hot_paths(&v)[0]["path"], "src/high.rs");
}

#[tokio::test]
async fn hot_path_audit_rejects_blank_and_duplicate_projects() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    sqlx::query("INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)")
        .bind("/ws/hot-dup-a")
        .bind("/ws/hot-dup-a/hot-dup")
        .bind("hot-dup")
        .execute(&pool)
        .await
        .expect("insert first duplicate project");
    sqlx::query("INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)")
        .bind("/ws/hot-dup-b")
        .bind("/ws/hot-dup-b/hot-dup")
        .bind("hot-dup")
        .execute(&pool)
        .await
        .expect("insert second duplicate project");
    let server = server_with_pool(pool);

    let blank = server
        .call_tool_cli("hot_path_audit", serde_json::json!({"project": "   "}))
        .await
        .expect_err("blank project must fail closed");
    assert!(
        blank.to_string().contains("project must be non-empty"),
        "unexpected blank-project error: {blank}"
    );

    let duplicate = server
        .call_tool_cli("hot_path_audit", serde_json::json!({"project": "hot-dup"}))
        .await
        .expect_err("duplicate project display names must fail closed");
    assert!(
        duplicate.to_string().contains("ambiguous project name"),
        "unexpected duplicate-project error: {duplicate}"
    );
}

#[tokio::test]
async fn hot_path_audit_ignores_cross_project_metric_rows() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let project_id = insert_project(&pool, "hot-scope", "/ws/hot-scope").await;
    let other_project_id = insert_project(&pool, "hot-other", "/ws/hot-other").await;
    let good = insert_file(
        &pool,
        project_id,
        "/ws/hot-scope/src/good.rs",
        "src/good.rs",
    )
    .await;
    let stale = insert_file(
        &pool,
        project_id,
        "/ws/hot-scope/src/stale.rs",
        "src/stale.rs",
    )
    .await;
    insert_metric(&pool, project_id, good, 0.1, 0.1, 0.1).await;
    // Deliberately inconsistent: file belongs to hot-scope, metric row belongs
    // to hot-other. It must not make `src/stale.rs` the hot path.
    insert_metric(&pool, other_project_id, stale, 100.0, 100.0, 100.0).await;

    let server = server_with_pool(pool);
    let result = server
        .call_tool_cli(
            "hot_path_audit",
            serde_json::json!({"project": "hot-scope", "percentile_threshold": 1.0}),
        )
        .await
        .expect("hot_path_audit call");
    let v: Value = serde_json::from_str(&text_of(&result)).expect("json");
    let paths: Vec<&str> = hot_paths(&v)
        .iter()
        .filter_map(|row| row["path"].as_str())
        .collect();
    assert!(
        paths.contains(&"src/good.rs"),
        "valid in-project metric row should rank: {v}"
    );
    assert!(
        !paths.contains(&"src/stale.rs"),
        "stale cross-project metric row leaked into hot paths: {v}"
    );
}
