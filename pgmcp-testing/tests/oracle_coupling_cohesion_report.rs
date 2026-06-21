//! Real-Postgres correctness oracle for `coupling_cohesion_report`.
//! At module_depth=1 the synthetic graph corpus has 3 modules:
//! `core/`, `util/`, `api/`.

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

#[tokio::test]
async fn coupling_cohesion_report_returns_modules_with_pinned_metrics() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_graph_corpus(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "coupling_cohesion_report",
            serde_json::json!({"project": "graph-proj", "module_depth": 1}),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    let modules = v["modules"].as_array().expect("modules");
    assert_eq!(
        modules.len(),
        3,
        "expected modules core/, util/, api/; got {}",
        modules.len()
    );
    let names: std::collections::BTreeSet<&str> = modules
        .iter()
        .map(|m| m["module"].as_str().unwrap_or(""))
        .collect();
    assert!(names.contains("core"));
    assert!(names.contains("util"));
    assert!(names.contains("api"));
}

#[tokio::test]
async fn coupling_cohesion_report_normalizes_and_validates_request() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_graph_corpus(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "coupling_cohesion_report",
            serde_json::json!({
                "project": " graph-proj ",
                "module_depth": -50,
                "sort_by": " coupling "
            }),
        )
        .await
        .expect("trimmed project and sort_by call");
    let v: Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(v["project"], "graph-proj");
    assert_eq!(v["module_depth"], 1);
    assert_eq!(v["sort_by"], "coupling");
    assert_eq!(v["truncated"], false);
    assert_eq!(v["module_count"], v["total_module_count"]);

    let err = server
        .call_tool_cli(
            "coupling_cohesion_report",
            serde_json::json!({"project": "graph-proj", "sort_by": "weight"}),
        )
        .await
        .expect_err("invalid sort_by must fail closed");
    assert!(
        err.to_string().contains("Unknown sort_by"),
        "unexpected invalid sort error: {err}"
    );
}

#[tokio::test]
async fn coupling_cohesion_report_rejects_duplicate_project_display_names() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    sqlx::query("INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)")
        .bind("/ws/cc-dup-a")
        .bind("/ws/cc-dup-a/cc-dup")
        .bind("cc-dup")
        .execute(&pool)
        .await
        .expect("insert first duplicate project");
    sqlx::query("INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)")
        .bind("/ws/cc-dup-b")
        .bind("/ws/cc-dup-b/cc-dup")
        .bind("cc-dup")
        .execute(&pool)
        .await
        .expect("insert second duplicate project");
    let server = server_with_pool(pool);

    let err = server
        .call_tool_cli(
            "coupling_cohesion_report",
            serde_json::json!({"project": "cc-dup"}),
        )
        .await
        .expect_err("duplicate project display names must fail closed");
    assert!(
        err.to_string().contains("ambiguous project name"),
        "unexpected duplicate project error: {err}"
    );
}

#[tokio::test]
async fn coupling_cohesion_report_ignores_cross_project_edges() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let project_id = insert_project(&pool, "cc-scope", "/ws/cc-scope").await;
    let other_project_id = insert_project(&pool, "cc-other", "/ws/cc-other").await;
    let source = insert_file(&pool, project_id, "/ws/cc-scope/src/a.rs", "src/a.rs").await;
    let foreign = insert_file(
        &pool,
        other_project_id,
        "/ws/cc-other/foreign/b.rs",
        "foreign/b.rs",
    )
    .await;

    // Both rows carry project_id = cc-scope, but at least one endpoint belongs
    // to cc-other. They must not add the foreign module or coupling edges.
    insert_import_edge(&pool, project_id, source, foreign).await;
    insert_import_edge(&pool, project_id, foreign, source).await;

    let server = server_with_pool(pool);
    let result = server
        .call_tool_cli(
            "coupling_cohesion_report",
            serde_json::json!({"project": "cc-scope", "module_depth": 1}),
        )
        .await
        .expect("coupling_cohesion_report call");
    let v: Value = serde_json::from_str(&text_of(&result)).expect("json");
    let modules = v["modules"].as_array().expect("modules");
    let names: std::collections::BTreeSet<&str> = modules
        .iter()
        .map(|m| m["module"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(names.len(), 1, "expected only the in-project module: {v}");
    assert!(names.contains("src"), "missing in-project module: {v}");
    assert!(
        !names.contains("foreign"),
        "foreign module leaked through stale cross-project edges: {v}"
    );
}

#[tokio::test]
async fn abstractness_reads_persisted_is_abstract_content_independently() {
    // The report sources module Abstractness from the persisted, symbol-derived
    // `file_metrics.is_abstract` — never from file content. A core module whose
    // file is marked abstract (content stays NULL) must report abstractness > 0.
    let db = require_test_db!();
    let pool = db.pool().clone();
    let project_id = insert_project(&pool, "cc-abs", "/ws/cc-abs").await;
    let a = insert_file(&pool, project_id, "/ws/cc-abs/core/t.rs", "core/t.rs").await;
    let b = insert_file(&pool, project_id, "/ws/cc-abs/util/u.rs", "util/u.rs").await;
    insert_import_edge(&pool, project_id, a, b).await;
    sqlx::query(
        "INSERT INTO file_metrics
         (file_id, project_id, pagerank, afferent_coupling, efferent_coupling, instability, is_abstract)
         VALUES ($1, $2, 1.0, 0, 1, 1.0, TRUE)",
    )
    .bind(a)
    .bind(project_id)
    .execute(&pool)
    .await
    .expect("persist abstract core file");
    sqlx::query(
        "INSERT INTO file_metrics
         (file_id, project_id, pagerank, afferent_coupling, efferent_coupling, instability, is_abstract)
         VALUES ($1, $2, 1.0, 1, 0, 0.0, FALSE)",
    )
    .bind(b)
    .bind(project_id)
    .execute(&pool)
    .await
    .expect("persist concrete util file");

    let server = server_with_pool(pool);
    let result = server
        .call_tool_cli(
            "coupling_cohesion_report",
            serde_json::json!({"project": "cc-abs", "module_depth": 1}),
        )
        .await
        .expect("call");
    let v: Value = serde_json::from_str(&text_of(&result)).expect("json");
    let core = v["modules"]
        .as_array()
        .expect("modules")
        .iter()
        .find(|m| m["module"] == "core")
        .unwrap_or_else(|| panic!("missing core module: {v}"));
    let abstractness: f64 = core["abstractness"]
        .as_str()
        .expect("abstractness")
        .parse()
        .expect("numeric");
    assert!(
        abstractness > 0.0,
        "core module abstractness must reflect persisted is_abstract: {v}"
    );
}
