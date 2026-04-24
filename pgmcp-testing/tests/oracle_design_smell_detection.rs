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
            serde_json::json!({"project": "graph-proj"}),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    let smells = v["smells"].as_array().expect("smells");
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
