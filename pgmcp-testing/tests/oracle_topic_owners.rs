//! Real-Postgres oracle for `topic_owners`.
//!
//! The synthetic corpus has no git blame, so the test stamps two authors
//! (alice ≈ 2/3, bob ≈ 1/3) onto `proj-auth`'s chunks and asserts the per-topic
//! ownership / bus-factor math is exercised.

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
async fn topic_owners_derives_bus_factor_from_blame() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    SyntheticCorpus::seed_with_assignments(&pool).await;
    // Stamp blame on proj-auth's chunks: alice owns the majority, bob a third.
    sqlx::query(
        "UPDATE file_chunks
         SET blame_author = CASE WHEN (id % 3 = 0) THEN 'bob' ELSE 'alice' END,
             blame_date = NOW()
         WHERE file_id IN (
             SELECT f.id FROM indexed_files f
             JOIN projects p ON p.id = f.project_id
             WHERE p.name = 'proj-auth'
         )",
    )
    .execute(&pool)
    .await
    .expect("stamp blame");
    let server = server_with_pool(pool);

    let result = server
        .call_tool_cli("topic_owners", serde_json::json!({"project": "proj-auth"}))
        .await
        .expect("topic_owners call");
    let v: serde_json::Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(v["project"], "proj-auth");
    let topics = v["topics"].as_array().expect("topics array");
    assert!(
        !topics.is_empty(),
        "blame populated → topics with owners: {v}"
    );

    // The dominant "auth" topic must carry author shares and a bus factor.
    let auth = topics
        .iter()
        .find(|t| t["label"] == "auth")
        .expect("auth topic present");
    assert!(
        auth["distinct_authors"].as_u64().expect("distinct") >= 1,
        "auth topic has ≥1 author: {v}"
    );
    assert!(
        auth["bus_factor"].as_u64().expect("bus_factor") >= 1,
        "bus_factor computed: {v}"
    );
    let top = &auth["top_authors"][0];
    let author = top["author"].as_str().expect("top author");
    assert!(
        author == "alice" || author == "bob",
        "top author is one of the stamped authors, got {author}: {v}"
    );
    let h = auth["herfindahl"].as_f64().expect("herfindahl");
    assert!(h > 0.0 && h <= 1.0, "herfindahl in (0,1]: {h}");
}
