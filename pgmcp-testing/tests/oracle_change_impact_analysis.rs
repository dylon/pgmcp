//! Real-Postgres correctness oracle for `change_impact_analysis`.
//! From core/a.rs the direct importers are core/c.rs (c → a) and
//! util/util.rs (util → a); BFS at depth 3 also picks up b
//! (b → c → a).

use crate::common::{server_with_pool, text_of};
use pgmcp_testing::fixtures::synthetic_corpus::seed_graph_corpus;
use pgmcp_testing::pool_tool_helpers::{seed_file, seed_file_symbol, seed_project};
use pgmcp_testing::require_test_db;
use serde_json::Value;
use uuid::Uuid;

#[tokio::test]
async fn change_impact_analysis_finds_direct_dependents_of_target_file() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_graph_corpus(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "change_impact_analysis",
            serde_json::json!({
                "project": "graph-proj",
                "file": "core/a.rs",
                "depth": 3,
                "include_semantic": false,
            }),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    let impacted = v["impacted_files"].as_array().expect("impacted");
    let paths: std::collections::BTreeSet<&str> = impacted
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert!(
        paths.contains("core/c.rs") && paths.contains("util/util.rs"),
        "direct dependents of a.rs (c.rs and util.rs) must appear in impact list; got {paths:?}"
    );
}

async fn insert_import_edge(pool: &sqlx::PgPool, project_id: i32, source: i64, target: i64) {
    sqlx::query(
        "INSERT INTO code_graph_edges (project_id, source_file_id, target_file_id, edge_type, weight)
         VALUES ($1, $2, $3, 'import', 1.0)",
    )
    .bind(project_id)
    .bind(source)
    .bind(target)
    .execute(pool)
    .await
    .expect("insert import edge");
}

async fn insert_resolved_call(
    pool: &sqlx::PgPool,
    source_file_id: i64,
    source_symbol_id: i64,
    target_symbol_id: i64,
    source_line: i32,
) {
    sqlx::query(
        "INSERT INTO symbol_references
            (source_file_id, source_symbol_id, target_symbol_id, target_raw,
             ref_kind, source_line, resolution_kind, resolution_confidence)
         VALUES ($1, $2, $3, 'target_fn', 'call', $4, 'exact_via_import', 0.95)",
    )
    .bind(source_file_id)
    .bind(source_symbol_id)
    .bind(target_symbol_id)
    .bind(source_line)
    .execute(pool)
    .await
    .expect("insert resolved call");
}

#[tokio::test]
async fn change_impact_analysis_rejects_duplicate_project_names() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let suffix = Uuid::new_v4().simple();
    let project_name = format!("impact-dup-{suffix}");
    seed_project(&pool, &project_name, &format!("/ws/{project_name}-a")).await;
    seed_project(&pool, &project_name, &format!("/ws/{project_name}-b")).await;
    let server = server_with_pool(pool);

    assert!(
        server
            .call_tool_cli(
                "change_impact_analysis",
                serde_json::json!({
                    "project": project_name,
                    "file": "a.rs",
                    "include_semantic": false,
                }),
            )
            .await
            .is_err(),
        "duplicate project names must fail closed"
    );
}

#[tokio::test]
async fn change_impact_analysis_scopes_import_call_and_effect_rows() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let suffix = Uuid::new_v4().simple();
    let target_name = format!("impact-target-{suffix}");
    let other_name = format!("impact-other-{suffix}");
    let target_path = format!("/ws/{target_name}");
    let other_path = format!("/ws/{other_name}");
    let target_project = seed_project(&pool, &target_name, &target_path).await;
    let other_project = seed_project(&pool, &other_name, &other_path).await;
    let target_file = seed_file(
        &pool,
        target_project,
        &format!("{target_path}/a.rs"),
        "a.rs",
    )
    .await;
    let same_project_dep = seed_file(
        &pool,
        target_project,
        &format!("{target_path}/b.rs"),
        "b.rs",
    )
    .await;
    let other_project_dep =
        seed_file(&pool, other_project, &format!("{other_path}/x.rs"), "x.rs").await;

    insert_import_edge(&pool, target_project, same_project_dep, target_file).await;
    insert_import_edge(&pool, other_project, other_project_dep, target_file).await;

    let target_symbol =
        seed_file_symbol(&pool, target_file, "target_fn", "function", 1, None).await;
    let other_symbol = seed_file_symbol(
        &pool,
        other_project_dep,
        "other_caller",
        "function",
        1,
        None,
    )
    .await;
    insert_resolved_call(&pool, other_project_dep, other_symbol, target_symbol, 7).await;

    sqlx::query("INSERT INTO symbol_effects (symbol_id, effect) VALUES ($1, $2)")
        .bind(other_symbol)
        .bind(pgmcp::parsing::type_tags::vocabulary::EFFECT_UNSAFE)
        .execute(&pool)
        .await
        .expect("seed other-project effect");

    let server = server_with_pool(pool);
    let result = server
        .call_tool_cli(
            "change_impact_analysis",
            serde_json::json!({
                "project": format!(" {target_name} "),
                "file": " a.rs ",
                "depth": 99,
                "include_semantic": false,
            }),
        )
        .await
        .expect("change impact call");
    let v: Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(v["depth"].as_i64(), Some(12), "depth is clamped");
    assert_eq!(v["project"].as_str(), Some(target_name.as_str()));
    assert_eq!(v["target_file"].as_str(), Some("a.rs"));

    let impacted = v["impacted_files"].as_array().expect("impacted");
    let paths: std::collections::BTreeSet<&str> = impacted
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert!(paths.contains("b.rs"), "same-project dependent is included");
    assert!(
        !paths.contains("x.rs"),
        "cross-project import/caller rows must not leak into impact list: {paths:?}"
    );
    assert!(
        v["effect_breakdown"].as_object().unwrap().is_empty(),
        "effect breakdown must not include another project's effects"
    );
}
