//! Real-Postgres correctness oracle for `architecture_violations`.
//!
//! Two assertions:
//!
//! 1. With no rules supplied, the tool must produce **zero** violations
//!    — there is nothing to violate. We explicitly require the
//!    `violations` array (or `violation_count`) to exist; a missing
//!    field is *not* acceptable as "no violations".
//! 2. With one explicit "api/ → util/ forbidden" rule, the tool must
//!    flag the planted api/api.rs → util/util.rs edge.

mod common;

use common::{server_with_pool, text_of};
use pgmcp_testing::fixtures::synthetic_corpus::seed_graph_corpus;
use pgmcp_testing::require_test_db;

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
async fn architecture_violations_no_rules_returns_zero_violations() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_graph_corpus(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "architecture_violations",
            serde_json::json!({"project": "graph-proj"}),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    let count = violation_count(&v);
    assert_eq!(
        count, 0,
        "no-rule run must produce 0 violations; got {count}\npayload: {v}"
    );
}

#[tokio::test]
async fn architecture_violations_detects_layer_violation() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_graph_corpus(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "architecture_violations",
            serde_json::json!({
                "project": "graph-proj",
                "rules": [
                    {
                        "from_pattern": "api/",
                        "to_pattern": "util/",
                        "kind": "forbidden",
                    }
                ],
            }),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    let count = violation_count(&v);
    assert!(
        count >= 1,
        "rule `api/ → util/ forbidden` must flag the planted edge; got {count}\npayload: {v}"
    );
}
