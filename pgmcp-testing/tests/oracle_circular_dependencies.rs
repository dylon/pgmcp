//! Real-Postgres correctness oracle for `circular_dependencies`.
//! The synthetic graph plants exactly two cycles: a 3-cycle in
//! core/ (a → b → c → a) and a 2-cycle util ↔ api.

mod common;

use common::{server_with_pool, text_of};
use pgmcp_testing::fixtures::synthetic_corpus::seed_graph_corpus;
use pgmcp_testing::require_test_db;

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
