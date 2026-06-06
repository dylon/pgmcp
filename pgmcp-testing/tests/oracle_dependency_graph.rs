//! Real-Postgres correctness oracle for `dependency_graph`.
//! See `pgmcp-testing/src/fixtures/synthetic_corpus.rs` for the
//! 5-file / 6-import-edge graph the assertions are derived from.

mod common;

use common::{server_with_pool, text_of};
use pgmcp_testing::fixtures::synthetic_corpus::seed_graph_corpus;
use pgmcp_testing::require_test_db;
use serde_json::Value;

#[tokio::test]
async fn dependency_graph_summary_reports_correct_node_and_edge_counts() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_graph_corpus(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "dependency_graph",
            serde_json::json!({"project": "graph-proj"}),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    // 5 nodes (a, b, c, util, api), 6 import edges.
    assert_eq!(v["node_count"], 5);
    assert_eq!(v["edge_count"], 6);
    // 6 edges form a single connected component (with both cycles).
    assert_eq!(v["components"], 1);
    let counts = v["edge_type_counts"].as_object().expect("counts");
    assert_eq!(counts["import"].as_u64(), Some(6));
}

#[tokio::test]
async fn dependency_graph_normalizes_and_validates_request_boundary() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_graph_corpus(&pool).await;
    let server = server_with_pool(pool);

    let focused = server
        .call_tool_cli(
            "dependency_graph",
            serde_json::json!({
                "project": " graph-proj ",
                "focus_file": " core/a.rs ",
                "depth": -10,
                "format": "summary",
                "edge_types": [" import ", "import"]
            }),
        )
        .await
        .expect("focused call");
    let v: Value = serde_json::from_str(&text_of(&focused)).expect("json");
    assert_eq!(v["project"], "graph-proj");
    assert_eq!(v["focus_file"], "core/a.rs");
    assert_eq!(v["depth"], 0);
    assert_eq!(v["node_count"], 1);
    assert_eq!(v["edge_count"], 0);
    assert_eq!(v["edge_types"].as_array().expect("edge_types").len(), 1);

    let invalid_format = server
        .call_tool_cli(
            "dependency_graph",
            serde_json::json!({"project": "graph-proj", "format": "xml"}),
        )
        .await
        .expect_err("invalid format must fail");
    assert!(
        invalid_format.to_string().contains("format must be one of"),
        "unexpected invalid-format error: {invalid_format}"
    );

    let invalid_edge_type = server
        .call_tool_cli(
            "dependency_graph",
            serde_json::json!({"project": "graph-proj", "edge_types": ["calls"]}),
        )
        .await
        .expect_err("invalid edge type must fail");
    assert!(
        invalid_edge_type
            .to_string()
            .contains("edge_type 'calls' is invalid"),
        "unexpected invalid-edge-type error: {invalid_edge_type}"
    );

    let missing_focus = server
        .call_tool_cli(
            "dependency_graph",
            serde_json::json!({"project": "graph-proj", "focus_file": "missing.rs"}),
        )
        .await
        .expect_err("missing focus must fail");
    assert!(
        missing_focus.to_string().contains("focus_file not found"),
        "unexpected missing-focus error: {missing_focus}"
    );
}

#[tokio::test]
async fn dependency_graph_rejects_duplicate_projects_and_stale_edges() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let h = seed_graph_corpus(&pool).await;

    let other_project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name)
         VALUES ('/ws/foreign', '/ws/foreign', 'foreign-proj')
         RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("insert foreign project");
    let foreign_file_id: i64 = sqlx::query_scalar(
        "INSERT INTO indexed_files
            (project_id, path, relative_path, language, size_bytes, line_count, modified_at)
         VALUES ($1, '/ws/foreign/lib.rs', 'lib.rs', 'rust', 10, 1, NOW())
         RETURNING id",
    )
    .bind(other_project_id)
    .fetch_one(&pool)
    .await
    .expect("insert foreign file");

    sqlx::query(
        "INSERT INTO code_graph_edges
            (project_id, source_file_id, target_file_id, edge_type, weight)
         VALUES ($1, $2, $3, 'import', 1.0)",
    )
    .bind(h.project_id)
    .bind(h.files["a"].0)
    .bind(foreign_file_id)
    .execute(&pool)
    .await
    .expect("insert stale cross-project edge");

    let server = server_with_pool(pool.clone());
    let result = server
        .call_tool_cli(
            "dependency_graph",
            serde_json::json!({"project": "graph-proj"}),
        )
        .await
        .expect("call");
    let v: Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(
        v["edge_count"], 6,
        "stale target-file project mismatch must not leak into the graph"
    );
    assert_eq!(v["node_count"], 5);

    sqlx::query(
        "INSERT INTO projects (workspace_path, path, name)
         VALUES ('/ws/graph-duplicate', '/ws/graph-duplicate', 'graph-proj')",
    )
    .execute(&pool)
    .await
    .expect("insert duplicate display name");

    let err = server
        .call_tool_cli(
            "dependency_graph",
            serde_json::json!({"project": "graph-proj"}),
        )
        .await
        .expect_err("duplicate project names must fail closed");
    assert!(
        err.to_string().contains("ambiguous project name"),
        "unexpected duplicate-project error: {err}"
    );
}
