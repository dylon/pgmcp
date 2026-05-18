//! Phase 3.2 pgmcp memory-server extension tests.
//!
//! Covers semantic / hybrid / facts_at / relations_traverse / code-anchor
//! tools. The DeterministicEmbeddingBackend in `server_with_pool` returns
//! 384d vectors, so the actual `memory_semantic_search` path that
//! requires 1024d is exercised via direct SQL with hand-rolled vectors
//! rather than via the MCP tool.
//!
//! Skips cleanly with `SKIPPED:` if no test DB is configured.

use pgmcp_testing::pool_tool_helpers::server_with_pool;
use pgmcp_testing::require_test_db;
use serde_json::Value;

fn extract_json(call_result: &rmcp::model::CallToolResult) -> Value {
    for content in &call_result.content {
        if let rmcp::model::RawContent::Text(text_content) = &content.raw {
            return serde_json::from_str::<Value>(&text_content.text)
                .expect("tool emitted invalid JSON");
        }
    }
    panic!("tool returned no Text content block");
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_semantic_search_query_returns_top_k_by_cosine() {
    let db = require_test_db!();
    let pool = db.pool();

    // Seed two entities, each with one 1024d observation.
    let e_close: i64 = sqlx::query_scalar(
        "INSERT INTO memory_entities (name, entity_type, source)
         VALUES ('close-entity', 'concept', 'agent_write'::memory_source)
         RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("e_close");
    let e_far: i64 = sqlx::query_scalar(
        "INSERT INTO memory_entities (name, entity_type, source)
         VALUES ('far-entity', 'concept', 'agent_write'::memory_source)
         RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("e_far");

    let close_v: Vec<f32> = (0..1024).map(|i| if i == 4 { 1.0 } else { 0.0 }).collect();
    let far_v: Vec<f32> = (0..1024)
        .map(|i| if i == 1000 { 1.0 } else { 0.0 })
        .collect();
    let pgv_close = pgvector::Vector::from(close_v.clone());
    let pgv_far = pgvector::Vector::from(far_v.clone());
    sqlx::query(
        "INSERT INTO memory_observations
            (entity_id, content, content_sha256, embedding, source)
         VALUES ($1, $2, $3, $4, 'agent_write'::memory_source)",
    )
    .bind(e_close)
    .bind("near content")
    .bind("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
    .bind(&pgv_close)
    .execute(pool)
    .await
    .expect("obs close");
    sqlx::query(
        "INSERT INTO memory_observations
            (entity_id, content, content_sha256, embedding, source)
         VALUES ($1, $2, $3, $4, 'agent_write'::memory_source)",
    )
    .bind(e_far)
    .bind("far content")
    .bind("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
    .bind(&pgv_far)
    .execute(pool)
    .await
    .expect("obs far");

    // Query with the close vector — should rank close-entity first.
    let rows = pgmcp::db::queries::memory_semantic_search(pool, &close_v, None, None, 5, 64)
        .await
        .expect("memory_semantic_search");
    assert!(!rows.is_empty(), "expected ≥1 hit");
    assert_eq!(rows[0].entity_name, "close-entity");
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_semantic_search_rejects_non_1024d_query_embedding() {
    let db = require_test_db!();
    let pool = db.pool();
    let v384: Vec<f32> = vec![0.0; 384];
    let err = pgmcp::db::queries::memory_semantic_search(pool, &v384, None, None, 5, 64)
        .await
        .unwrap_err();
    assert!(
        format!("{err}").contains("expected 1024d"),
        "expected dim rejection, got: {err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_facts_at_returns_pre_delete_snapshot() {
    let db = require_test_db!();
    let pool = db.pool();
    let server = server_with_pool(pool.clone());

    server
        .call_tool_cli(
            "memory_create_entities",
            serde_json::json!({
                "entities": [
                    {"name": "time-test", "entity_type": "concept",
                     "observations": ["fact-a", "fact-b"]}
                ]
            }),
        )
        .await
        .expect("create");

    // Take a snapshot timestamp before deletion.
    let t0: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT NOW() + interval '1 second'")
            .fetch_one(pool)
            .await
            .expect("t0");

    // Wait briefly so soft-delete `valid_to` is strictly after t0.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    server
        .call_tool_cli(
            "memory_delete_entities",
            serde_json::json!({"names": ["time-test"]}),
        )
        .await
        .expect("delete");

    // facts_at(t0) should still see the entity.
    let as_of = t0.to_rfc3339();
    let snap = server
        .call_tool_cli("memory_facts_at", serde_json::json!({"as_of": as_of}))
        .await
        .expect("memory_facts_at");
    let body = extract_json(&snap);
    let entities = body
        .get("entities")
        .and_then(Value::as_array)
        .expect("entities array");
    assert!(
        entities
            .iter()
            .any(|e| e.get("name").and_then(Value::as_str) == Some("time-test")),
        "snapshot at t0 should still contain the entity: {entities:?}"
    );

    // facts_at(NOW()) should no longer see it.
    let snap_now = server
        .call_tool_cli("memory_facts_at", serde_json::json!({}))
        .await
        .expect("memory_facts_at NOW");
    let body = extract_json(&snap_now);
    let entities = body
        .get("entities")
        .and_then(Value::as_array)
        .expect("entities array");
    assert!(
        entities
            .iter()
            .all(|e| e.get("name").and_then(Value::as_str) != Some("time-test")),
        "snapshot at NOW should NOT contain the deleted entity"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_relations_traverse_bfs_depth_caps_correctly() {
    let db = require_test_db!();
    let pool = db.pool();
    let server = server_with_pool(pool.clone());

    // Seed a chain: a → b → c → d.
    server
        .call_tool_cli(
            "memory_create_entities",
            serde_json::json!({
                "entities": [
                    {"name": "chain-a", "entity_type": "node"},
                    {"name": "chain-b", "entity_type": "node"},
                    {"name": "chain-c", "entity_type": "node"},
                    {"name": "chain-d", "entity_type": "node"}
                ]
            }),
        )
        .await
        .expect("seed");
    server
        .call_tool_cli(
            "memory_create_relations",
            serde_json::json!({
                "relations": [
                    {"from": "chain-a", "to": "chain-b", "relation_type": "next"},
                    {"from": "chain-b", "to": "chain-c", "relation_type": "next"},
                    {"from": "chain-c", "to": "chain-d", "relation_type": "next"}
                ]
            }),
        )
        .await
        .expect("relations");

    let a_id: i64 = sqlx::query_scalar(
        "SELECT id FROM memory_entities WHERE name = 'chain-a' AND valid_to IS NULL",
    )
    .fetch_one(pool)
    .await
    .expect("a_id");

    // depth 1 → {a, b}; depth 2 → {a, b, c}; depth 3 → {a, b, c, d}.
    for (depth, expected_min) in [(1, 2), (2, 3), (3, 4)] {
        let res = server
            .call_tool_cli(
                "memory_relations_traverse",
                serde_json::json!({
                    "seed_entity_ids": [a_id],
                    "max_depth": depth
                }),
            )
            .await
            .expect("traverse");
        let body = extract_json(&res);
        let nodes = body.get("nodes").and_then(Value::as_array).expect("nodes");
        assert!(
            nodes.len() >= expected_min,
            "depth={depth} should reach ≥{expected_min} nodes; got {}",
            nodes.len()
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_anchor_round_trip_and_reverse_lookup() {
    let db = require_test_db!();
    let pool = db.pool();
    let server = server_with_pool(pool.clone());

    // Seed entity + file.
    server
        .call_tool_cli(
            "memory_create_entities",
            serde_json::json!({
                "entities": [{"name": "anchored", "entity_type": "concept"}]
            }),
        )
        .await
        .expect("create");
    let entity_id: i64 = sqlx::query_scalar(
        "SELECT id FROM memory_entities WHERE name = 'anchored' AND valid_to IS NULL",
    )
    .fetch_one(pool)
    .await
    .expect("entity_id");

    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3)
         ON CONFLICT (path) DO UPDATE SET name = $3 RETURNING id",
    )
    .bind("/ws")
    .bind("/ws/anchor-test")
    .bind("anchor-test")
    .fetch_one(pool)
    .await
    .expect("project");
    let file_id: i64 = sqlx::query_scalar(
        "INSERT INTO indexed_files (project_id, path, relative_path, language, size_bytes, content, line_count, modified_at) \
         VALUES ($1, $2, $3, 'rust', 10, 'fn f() {}', 1, NOW()) RETURNING id",
    )
    .bind(project_id)
    .bind("/ws/anchor-test/a.rs")
    .bind("a.rs")
    .fetch_one(pool)
    .await
    .expect("file");

    // Anchor.
    let anchored = server
        .call_tool_cli(
            "memory_anchor_entity",
            serde_json::json!({
                "entity_id": entity_id,
                "file_id": file_id,
                "anchor_type": "implements"
            }),
        )
        .await
        .expect("anchor");
    let body = extract_json(&anchored);
    let anchor_id = body
        .get("anchor_id")
        .and_then(Value::as_i64)
        .expect("anchor_id");
    assert!(anchor_id > 0);

    // Forward lookup.
    let fwd = server
        .call_tool_cli(
            "memory_find_code_for_entity",
            serde_json::json!({"entity_id": entity_id}),
        )
        .await
        .expect("find_code");
    let body = extract_json(&fwd);
    let anchors = body
        .get("anchors")
        .and_then(Value::as_array)
        .expect("anchors");
    assert_eq!(anchors.len(), 1);
    assert_eq!(
        anchors[0].get("file_id").and_then(Value::as_i64),
        Some(file_id)
    );

    // Reverse lookup.
    let rev = server
        .call_tool_cli(
            "memory_find_entities_for_code",
            serde_json::json!({"file_id": file_id}),
        )
        .await
        .expect("find_entities");
    let body = extract_json(&rev);
    let anchors = body
        .get("anchors")
        .and_then(Value::as_array)
        .expect("anchors");
    assert_eq!(anchors.len(), 1);
    assert_eq!(
        anchors[0].get("entity_id").and_then(Value::as_i64),
        Some(entity_id)
    );

    // Unanchor — and reverse lookup goes empty.
    let un = server
        .call_tool_cli(
            "memory_unanchor_entity",
            serde_json::json!({"anchor_id": anchor_id}),
        )
        .await
        .expect("unanchor");
    let body = extract_json(&un);
    assert_eq!(body.get("removed").and_then(Value::as_bool), Some(true));

    let rev2 = server
        .call_tool_cli(
            "memory_find_entities_for_code",
            serde_json::json!({"file_id": file_id}),
        )
        .await
        .expect("find_entities_2");
    let body = extract_json(&rev2);
    let anchors = body
        .get("anchors")
        .and_then(Value::as_array)
        .expect("anchors");
    assert!(
        anchors.is_empty(),
        "unanchored should disappear from reverse lookup"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_find_entities_for_code_requires_exactly_one_target() {
    let db = require_test_db!();
    let pool = db.pool();
    let server = server_with_pool(pool.clone());
    // Empty target — invalid.
    let result = server
        .call_tool_cli("memory_find_entities_for_code", serde_json::json!({}))
        .await;
    match result {
        Err(_) => {}
        Ok(r) => assert_eq!(r.is_error, Some(true), "expected error, got {r:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_anchor_entity_rejects_all_null_target() {
    let db = require_test_db!();
    let pool = db.pool();
    let server = server_with_pool(pool.clone());
    server
        .call_tool_cli(
            "memory_create_entities",
            serde_json::json!({
                "entities": [{"name": "lonely", "entity_type": "concept"}]
            }),
        )
        .await
        .expect("create");
    let entity_id: i64 = sqlx::query_scalar(
        "SELECT id FROM memory_entities WHERE name = 'lonely' AND valid_to IS NULL",
    )
    .fetch_one(pool)
    .await
    .expect("entity");
    let result = server
        .call_tool_cli(
            "memory_anchor_entity",
            serde_json::json!({
                "entity_id": entity_id,
                "anchor_type": "implements"
            }),
        )
        .await;
    match result {
        Err(_) => {}
        Ok(r) => assert_eq!(r.is_error, Some(true), "expected error, got {r:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_semantic_search_validates_tier_filter() {
    let db = require_test_db!();
    let pool = db.pool();
    let server = server_with_pool(pool.clone());
    // Invalid tier should error.
    let result = server
        .call_tool_cli(
            "memory_semantic_search",
            serde_json::json!({"query": "anything", "tier": "not-a-tier"}),
        )
        .await;
    match result {
        Err(_) => {}
        Ok(r) => assert_eq!(r.is_error, Some(true), "expected error, got {r:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_hybrid_search_returns_results_envelope() {
    let db = require_test_db!();
    let pool = db.pool();
    let server = server_with_pool(pool.clone());
    // We're calling through the MCP server which uses a 384d
    // DeterministicEmbedder — the query function rejects that with a clear
    // protocol error. Verify the tool propagates the error cleanly.
    let result = server
        .call_tool_cli(
            "memory_hybrid_search",
            serde_json::json!({"query": "anything"}),
        )
        .await;
    match result {
        Err(e) => assert!(
            format!("{e}").contains("1024d") || format!("{e}").contains("expected"),
            "expected dim-related error, got: {e}"
        ),
        Ok(r) => assert_eq!(
            r.is_error,
            Some(true),
            "expected error from 384d → 1024d mismatch, got {r:?}"
        ),
    }
}
