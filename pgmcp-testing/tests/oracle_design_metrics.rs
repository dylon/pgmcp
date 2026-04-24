//! Real-Postgres correctness oracle for `design_metrics`.
//! 3 first-level modules (core/, util/, api/) each with the Martin
//! triple Ca/Ce/I + abstractness + distance.

mod common;

use common::{server_with_pool, text_of};
use pgmcp_testing::fixtures::synthetic_corpus::seed_graph_corpus;
use pgmcp_testing::require_test_db;

#[tokio::test]
async fn design_metrics_reports_per_module_martin_metrics() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_graph_corpus(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "design_metrics",
            serde_json::json!({"project": "graph-proj", "module_depth": 1}),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    let modules = v["modules"].as_array().expect("modules");
    assert_eq!(
        modules.len(),
        3,
        "3 first-level modules (core/, util/, api/); got {}",
        modules.len()
    );
    for m in modules {
        assert!(m.get("afferent_coupling").is_some());
        assert!(m.get("efferent_coupling").is_some());
        assert!(m.get("instability").is_some());
        assert!(m.get("abstractness").is_some());
        assert!(m.get("distance").is_some());
    }
}
