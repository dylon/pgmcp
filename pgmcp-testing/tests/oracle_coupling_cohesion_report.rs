//! Real-Postgres correctness oracle for `coupling_cohesion_report`.
//! At module_depth=1 the synthetic graph corpus has 3 modules:
//! `core/`, `util/`, `api/`.

mod common;

use common::{server_with_pool, text_of};
use pgmcp_testing::fixtures::synthetic_corpus::seed_graph_corpus;
use pgmcp_testing::require_test_db;

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
