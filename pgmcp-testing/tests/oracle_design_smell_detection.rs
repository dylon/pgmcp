//! Real-Postgres correctness oracle for `design_smell_detection`.
//!
//! Boost util/util.rs's in_degree to 10 (the synthetic corpus seeds
//! it at 1, with churn 8.0/month) so the `unstable_dependency`
//! heuristic (in_degree > 5 AND churn_rate > 2.0) fires.

mod common;

use common::{server_with_pool, text_of};
use pgmcp_testing::fixtures::synthetic_corpus::seed_graph_corpus;
use pgmcp_testing::require_test_db;

#[tokio::test]
async fn design_smell_detection_flags_unstable_dependency_for_high_churn_file() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let h = seed_graph_corpus(&pool).await;

    sqlx::query("UPDATE file_metrics SET in_degree = 10 WHERE file_id = $1")
        .bind(h.files["util"].0)
        .execute(&pool)
        .await
        .expect("bump");

    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "design_smell_detection",
            serde_json::json!({
                "project": " graph-proj ",
                "smells": [" unstable_dependency ", "unstable_dependency"],
                "limit": -20
            }),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(v["project"], "graph-proj");
    assert_eq!(v["limit"], 1);
    assert_eq!(v["detect_all"], false);
    assert_eq!(
        v["smells_requested"].as_array().expect("requested").len(),
        1
    );
    let smells = v["smells"].as_array().expect("smells");
    assert_eq!(smells.len(), 1, "limit should clamp to one result");
    let unstable: Vec<_> = smells
        .iter()
        .filter(|s| s["smell"].as_str() == Some("unstable_dependency"))
        .collect();
    assert!(
        unstable
            .iter()
            .any(|s| s["path"].as_str() == Some("util/util.rs")),
        "expected unstable_dependency smell on util/util.rs; got smells {smells:?}"
    );
}

#[tokio::test]
async fn design_smell_detection_rejects_invalid_smells_and_duplicate_projects() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_graph_corpus(&pool).await;
    let server = server_with_pool(pool.clone());

    let invalid_smell = server
        .call_tool_cli(
            "design_smell_detection",
            serde_json::json!({"project": "graph-proj", "smells": ["mystery"]}),
        )
        .await
        .expect_err("invalid smell must fail");
    assert!(
        invalid_smell
            .to_string()
            .contains("smell 'mystery' is invalid"),
        "unexpected invalid-smell error: {invalid_smell}"
    );

    sqlx::query(
        "INSERT INTO projects (workspace_path, path, name)
         VALUES ('/ws/design-smell-duplicate', '/ws/design-smell-duplicate', 'graph-proj')",
    )
    .execute(&pool)
    .await
    .expect("insert duplicate display name");

    let duplicate_project = server
        .call_tool_cli(
            "design_smell_detection",
            serde_json::json!({"project": "graph-proj"}),
        )
        .await
        .expect_err("duplicate project names must fail closed");
    assert!(
        duplicate_project
            .to_string()
            .contains("ambiguous project name"),
        "unexpected duplicate-project error: {duplicate_project}"
    );
}

#[tokio::test]
async fn design_smell_detection_ignores_stale_cross_project_metric_rows() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let h = seed_graph_corpus(&pool).await;

    let foreign_project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name)
         VALUES ('/ws/design-smell-foreign', '/ws/design-smell-foreign', 'foreign-smell-proj')
         RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("insert foreign project");

    sqlx::query(
        "UPDATE file_metrics
         SET project_id = $1, in_degree = 99, churn_rate = 99.0
         WHERE file_id = $2",
    )
    .bind(foreign_project_id)
    .bind(h.files["util"].0)
    .execute(&pool)
    .await
    .expect("make util metric stale");

    let server = server_with_pool(pool);
    let result = server
        .call_tool_cli(
            "design_smell_detection",
            serde_json::json!({
                "project": "graph-proj",
                "smells": ["unstable_dependency"]
            }),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    let smells = v["smells"].as_array().expect("smells");
    assert!(
        smells.is_empty(),
        "stale foreign-project metric row must not trigger smells: {smells:?}"
    );
}
