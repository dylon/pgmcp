//! Real-Postgres test for the project-dependency tools + the unified-graph
//! integration: an upserted `project_dependencies` edge is queryable both
//! directions (`project_dependents` / `project_dependencies`) and surfaces as a
//! `project_depends_on` edge in `memory_unified_edges`. Satisfies the coverage
//! gate for both tools via the literal `call_tool_cli("…")`.

use pgmcp::deps::DepSource;
use pgmcp::deps::store::upsert_dependency;
use pgmcp_testing::pool_tool_helpers::{seed_project, server_with_pool};
use pgmcp_testing::require_test_db;

fn tool_json(result: &rmcp::model::CallToolResult) -> serde_json::Value {
    let text = result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .next()
        .expect("text content present");
    serde_json::from_str(&text).expect("tool output is JSON")
}

#[tokio::test(flavor = "multi_thread")]
async fn project_deps_tools_and_graph_edge() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let d = seed_project(&pool, "consumer-D", "/ws/consumer-D").await;
    let u = seed_project(&pool, "dependency-U", "/ws/dependency-U").await;

    // D depends on U (a Cargo path dep).
    upsert_dependency(
        &pool,
        d,
        u,
        Some("dependency-U"),
        Some("path"),
        DepSource::Cargo,
        1.0,
    )
    .await
    .expect("upsert dependency");

    let server = server_with_pool(pool.clone());

    // Reverse: dependents of U → D.
    let res = server
        .call_tool_cli(
            "project_dependents",
            serde_json::json!({ "project": "dependency-U" }),
        )
        .await
        .expect("project_dependents ok");
    let j = tool_json(&res);
    assert_eq!(j["dependent_count"], 1);
    assert_eq!(j["dependents"][0]["project"], "consumer-D");

    // Forward: dependencies of D → U.
    let res = server
        .call_tool_cli(
            "project_dependencies",
            serde_json::json!({ "project": "consumer-D" }),
        )
        .await
        .expect("project_dependencies ok");
    let j = tool_json(&res);
    assert_eq!(j["dependency_count"], 1);
    assert_eq!(j["dependencies"][0]["project"], "dependency-U");
    assert_eq!(j["dependencies"][0]["kind"], "path");
    assert_eq!(j["dependencies"][0]["source"], "cargo");

    // Graph integration: the bitemporal edge surfaces in the unified graph.
    sqlx::query("REFRESH MATERIALIZED VIEW memory_unified_edges")
        .execute(&pool)
        .await
        .expect("refresh unified edges");
    let cnt: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM memory_unified_edges
          WHERE edge_type = 'project_depends_on' AND from_id = $1 AND to_id = $2",
    )
    .bind(format!("project:{d}"))
    .bind(format!("project:{u}"))
    .fetch_one(&pool)
    .await
    .expect("count edges");
    assert_eq!(cnt, 1, "project_depends_on edge present in unified graph");
}
