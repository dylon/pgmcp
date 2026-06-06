//! Real-Postgres correctness oracle for `circular_dependencies`.
//! The synthetic graph plants exactly two cycles: a 3-cycle in
//! core/ (a → b → c → a) and a 2-cycle util ↔ api.

mod common;

use common::{server_with_pool, text_of};
use pgmcp_testing::fixtures::synthetic_corpus::seed_graph_corpus;
use pgmcp_testing::require_test_db;
use uuid::Uuid;

#[tokio::test]
async fn circular_dependencies_finds_both_planted_cycles() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_graph_corpus(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "circular_dependencies",
            serde_json::json!({"project": "graph-proj"}),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    // Two SCCs are non-trivial (size ≥ 2): {a,b,c} and {util, api}.
    let scc_count = v["scc_count"].as_u64().expect("scc_count");
    assert!(
        scc_count >= 2,
        "expected ≥ 2 non-trivial SCCs (3-cycle + 2-cycle); got {scc_count}"
    );
    let cycles = v["cycles"].as_array().expect("cycles");
    let lengths: std::collections::BTreeSet<u64> = cycles
        .iter()
        .map(|c| c["length"].as_u64().unwrap_or(0))
        .collect();
    assert!(
        lengths.contains(&2),
        "missing 2-cycle in extracted cycles: {lengths:?}"
    );
    assert!(
        lengths.contains(&3),
        "missing 3-cycle in extracted cycles: {lengths:?}"
    );
}

#[tokio::test]
async fn circular_dependencies_clamps_negative_cycle_length() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_graph_corpus(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "circular_dependencies",
            serde_json::json!({"project": "graph-proj", "max_cycle_length": -10}),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");

    assert_eq!(v["max_cycle_length"].as_u64(), Some(2));
    for cycle in v["cycles"].as_array().expect("cycles") {
        assert!(
            cycle["length"].as_u64().expect("cycle length") <= 2,
            "negative max_cycle_length must not expand the search space: {cycle}"
        );
    }
}

#[tokio::test]
async fn circular_dependencies_caps_large_cycle_length() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_graph_corpus(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "circular_dependencies",
            serde_json::json!({"project": "graph-proj", "max_cycle_length": 500}),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");

    assert_eq!(v["max_cycle_length"].as_u64(), Some(64));
    let lengths: std::collections::BTreeSet<u64> = v["cycles"]
        .as_array()
        .expect("cycles")
        .iter()
        .map(|c| c["length"].as_u64().unwrap_or(0))
        .collect();
    assert!(
        lengths.contains(&2) && lengths.contains(&3),
        "capping a large max_cycle_length should preserve planted cycles: {lengths:?}"
    );
}

#[tokio::test]
async fn circular_dependencies_rejects_ambiguous_project_name() {
    let db = require_test_db!();
    let name = format!("duplicate-cycles-{}", Uuid::now_v7().simple());
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
            "circular_dependencies",
            serde_json::json!({"project": name}),
        )
        .await
        .expect_err("duplicate project display names must fail closed");

    assert!(
        err.to_string().contains("ambiguous project name"),
        "unexpected circular_dependencies ambiguity error: {err}"
    );
}
