//! Integration test for the new-corpus topic models (ADR-029, item 14):
//! work_item_topics / commit_topics / prompt_topics. Seeds two separable
//! work-item embedding clusters and asserts they cluster; exercises all three
//! dispatched tools via call_tool_cli (Layer-D coverage gate).

mod common;

use common::{server_with_pool, text_of};
use pgmcp_testing::require_test_db;
use serde_json::json;

fn body(r: &rmcp::model::CallToolResult) -> serde_json::Value {
    serde_json::from_str(&text_of(r)).expect("tool body must be JSON")
}

fn emb(axis: usize, jitter: f32) -> pgvector::Vector {
    let mut v = vec![0.0f32; 1024];
    v[axis] = 1.0;
    v[axis + 1] = jitter;
    pgvector::Vector::from(v)
}

#[tokio::test]
async fn corpus_topics_cluster_work_items() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let server = server_with_pool(pool.clone());

    // Two separable embedding clusters of bugs.
    for i in 0..6 {
        sqlx::query(
            "INSERT INTO work_items (public_id, kind, status, title, embedding)
             VALUES ($1, 'bug', 'triage', 'authentication login token failure', $2)
             ON CONFLICT (public_id) DO UPDATE SET embedding = $2",
        )
        .bind(format!("ct-auth-{i}"))
        .bind(emb(0, 0.02 * i as f32))
        .execute(&pool)
        .await
        .expect("seed auth bug");
    }
    for i in 0..6 {
        sqlx::query(
            "INSERT INTO work_items (public_id, kind, status, title, embedding)
             VALUES ($1, 'bug', 'triage', 'database query index timeout', $2)
             ON CONFLICT (public_id) DO UPDATE SET embedding = $2",
        )
        .bind(format!("ct-db-{i}"))
        .bind(emb(200, 0.02 * i as f32))
        .execute(&pool)
        .await
        .expect("seed db bug");
    }

    // work_item_topics over the bug corpus → at least one theme covering all 12.
    let wi = body(
        &server
            .call_tool_cli("work_item_topics", json!({"k": 2, "kind": "bug"}))
            .await
            .expect("work_item_topics"),
    );
    assert!(wi["items"].as_i64().unwrap() >= 12, "{wi}");
    assert!(wi["topic_count"].as_i64().unwrap() >= 1, "{wi}");
    let covered: i64 = wi["topics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["size"].as_i64().unwrap_or(0))
        .sum();
    assert!(covered >= 12, "every item assigned to a theme: {wi}");

    // commit_topics + prompt_topics: exercised for coverage (empty corpora ok).
    let ct = body(
        &server
            .call_tool_cli("commit_topics", json!({"k": 4}))
            .await
            .expect("commit_topics"),
    );
    assert_eq!(ct["corpus"], "git_commits");
    let pt = body(
        &server
            .call_tool_cli("prompt_topics", json!({"k": 4}))
            .await
            .expect("prompt_topics"),
    );
    assert_eq!(pt["corpus"], "prompts");
}
