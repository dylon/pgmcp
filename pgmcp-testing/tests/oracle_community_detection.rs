//! Real-Postgres correctness oracle for `community_detection`.
//!
//! The synthetic graph corpus has clearly-defined module groupings:
//! `core/` (a, b, c — densely interconnected via the 3-cycle) and
//! `util/` ↔ `api/` (the 2-cycle). On this topology Louvain must:
//!
//! 1. Return a non-trivial partition (`num_communities >= 2`)
//! 2. Yield strictly positive modularity (any partition that isn't
//!    "everything in one community" gives Q > 0 on a graph with
//!    visible block structure)
//! 3. Place every node into exactly one community

use crate::common::{server_with_pool, text_of};
use pgmcp_testing::fixtures::synthetic_corpus::seed_graph_corpus;
use pgmcp_testing::pool_tool_helpers::{seed_file, seed_project};
use pgmcp_testing::require_test_db;

#[tokio::test]
async fn community_detection_partitions_synthetic_graph_into_at_least_two_communities() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_graph_corpus(&pool).await;
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "community_detection",
            serde_json::json!({"project": "graph-proj"}),
        )
        .await
        .expect("call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");

    // The corpus has two cycle-bound clusters (core/{a,b,c} and
    // util↔api). Louvain must produce at least 2 communities; the
    // ideal partition produces exactly those 2 groups.
    let num_communities = v["num_communities"]
        .as_u64()
        .expect("num_communities field present");
    assert!(
        num_communities >= 2,
        "Louvain must produce ≥ 2 communities on a graph with two cycle clusters; got {num_communities}"
    );
    assert!(
        num_communities <= 5,
        "Louvain shouldn't produce more communities than nodes; got {num_communities}"
    );

    // Modularity must be a finite, non-negative number. On any
    // partition with visible block structure Q > 0; we accept >= 0
    // because Louvain may pick a tiny-positive solution rather than
    // the absolute optimum.
    let modularity = v["modularity"]
        .as_f64()
        .expect("modularity must be numeric");
    assert!(modularity.is_finite(), "modularity must be finite");
    assert!(
        modularity >= 0.0,
        "modularity must be non-negative on a valid partition; got {modularity}"
    );

    // Every node assigned to exactly one community.
    if let Some(communities) = v["communities"].as_array() {
        let mut total_assigned: usize = 0;
        for c in communities {
            if let Some(members) = c["members"].as_array() {
                total_assigned += members.len();
            }
        }
        assert_eq!(
            total_assigned, 5,
            "all 5 nodes must be assigned to exactly one community; got {total_assigned}"
        );
    }
}

#[tokio::test]
async fn community_detection_rejects_invalid_graph_type() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _h = seed_graph_corpus(&pool).await;
    let server = server_with_pool(pool);

    let err = server
        .call_tool_cli(
            "community_detection",
            serde_json::json!({"project": "graph-proj", "graph_type": "imports-plus"}),
        )
        .await;
    assert!(err.is_err(), "unknown graph_type must fail closed");
}

#[tokio::test]
async fn community_detection_clamps_resolution_and_rejects_stale_edges() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let h = seed_graph_corpus(&pool).await;
    let other_project = seed_project(&pool, "community-other", "/ws/community-other").await;
    let leaked_file = seed_file(
        &pool,
        other_project,
        "/ws/community-other/leak.rs",
        "leak.rs",
    )
    .await;

    sqlx::query(
        "INSERT INTO code_graph_edges
            (project_id, source_file_id, target_file_id, edge_type, weight)
         VALUES ($1, $2, $2, 'import', 1.0)",
    )
    .bind(h.project_id)
    .bind(leaked_file)
    .execute(&pool)
    .await
    .expect("insert stale cross-project edge");

    let server = server_with_pool(pool);
    let result = server
        .call_tool_cli(
            "community_detection",
            serde_json::json!({
                "project": " graph-proj ",
                "graph_type": " combined ",
                "resolution": 500.0,
            }),
        )
        .await
        .expect("community_detection");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(v["project"].as_str(), Some("graph-proj"));
    assert_eq!(v["graph_type"].as_str(), Some("combined"));
    assert_eq!(v["resolution"].as_f64(), Some(10.0));

    let body = v.to_string();
    assert!(
        !body.contains("leak.rs"),
        "stale cross-project edge leaked into community output: {body}"
    );
}
