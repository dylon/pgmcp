//! Real-Postgres correctness oracle for `change_impact_analysis`.
//! From core/a.rs the direct importers are core/c.rs (c → a) and
//! util/util.rs (util → a); BFS at depth 3 also picks up b
//! (b → c → a).

mod common;

use common::{server_with_pool, text_of};
use pgmcp_testing::fixtures::synthetic_corpus::seed_graph_corpus;
use pgmcp_testing::require_test_db;

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
