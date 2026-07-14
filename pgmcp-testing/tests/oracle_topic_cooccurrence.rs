//! Real-Postgres oracle for `topic_cooccurrence`.
//!
//! The synthetic corpus assigns each chunk to exactly one topic, so it has no
//! co-occurrences. The test injects a few dual memberships (auth chunks also in
//! the database topic) to create one topic-pair edge and asserts the graph +
//! Louvain pipeline picks it up. (The graph/bridge logic itself is unit-tested
//! in `topic_analysis::cooccurrence`.)

use crate::common::server_with_pool;
use pgmcp_testing::fixtures::synthetic_corpus::SyntheticCorpus;
use pgmcp_testing::require_test_db;

fn text_of(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .next()
        .expect("text content present")
}

#[tokio::test]
async fn topic_cooccurrence_builds_graph_from_co_assignments() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let h = SyntheticCorpus::seed_with_assignments(&pool).await;
    let planted = h.planted_topics.expect("planted topics");

    // Dual-assign three auth chunks to the database topic → an auth↔database edge.
    for &cid in h.auth_chunk_ids.iter().take(3) {
        sqlx::query(
            "INSERT INTO chunk_topic_assignments (chunk_id, topic_id, membership_score)
             VALUES ($1, $2, 0.5)
             ON CONFLICT (chunk_id, topic_id) DO NOTHING",
        )
        .bind(cid)
        .bind(planted.database)
        .execute(&pool)
        .await
        .expect("inject co-assignment");
    }
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli(
            "topic_cooccurrence",
            serde_json::json!({"project": "proj-auth", "min_weight": 2}),
        )
        .await
        .expect("topic_cooccurrence call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(v["project"], "proj-auth");
    assert!(
        v["n_edges"].as_u64().expect("n_edges") >= 1,
        "the injected co-assignment must produce an edge: {v}"
    );
    assert!(
        v["modularity"].as_f64().expect("modularity").is_finite(),
        "modularity must be finite: {v}"
    );
    assert!(
        !v["communities"].as_array().expect("communities").is_empty(),
        "a connected topic pair forms a community: {v}"
    );
}
