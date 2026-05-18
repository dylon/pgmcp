//! Phase 2 (memory schema) + Phase 3.1 (official-compat CRUD) integration
//! tests.
//!
//! Covers:
//! - All Phase-2 tables present with bi-temporal columns and CHECK
//!   constraints honored.
//! - The 9 official-compat tools wired through the MCP server roundtrip
//!   correctly (create → search → open → delete → re-read).
//! - Soft-delete via `valid_to = NOW()` is the default behavior; deleted
//!   rows are no longer surfaced by the default-active queries.
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
async fn phase2_tables_exist_with_bi_temporal_columns() {
    let db = require_test_db!();
    let pool = db.pool();
    for table in [
        "memory_scope",
        "memory_entities",
        "memory_entity_scope",
        "memory_entity_tier",
        "memory_observations",
        "memory_relations",
        "memory_code_anchor",
        "memory_summary_tree",
        "memory_forget_log",
        "memory_reflection_runs",
    ] {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM information_schema.tables
                            WHERE table_schema = 'public' AND table_name = $1)",
        )
        .bind(table)
        .fetch_one(pool)
        .await
        .expect("table exists query");
        assert!(exists, "table {} should exist after migrations", table);
    }

    // Spot-check bi-temporal columns on memory_entities.
    for col in ["valid_from", "valid_to", "superseded_by"] {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM information_schema.columns
                            WHERE table_name = 'memory_entities' AND column_name = $1)",
        )
        .bind(col)
        .fetch_one(pool)
        .await
        .expect("column exists query");
        assert!(exists, "memory_entities.{} should exist", col);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_code_anchor_rejects_all_null_fks() {
    let db = require_test_db!();
    let pool = db.pool();

    // Seed an entity so we have a valid entity_id.
    let entity_id: i64 = sqlx::query_scalar(
        "INSERT INTO memory_entities (name, entity_type, source)
         VALUES ('test-entity', 'concept', 'agent_write'::memory_source)
         RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("insert entity");

    // All-NULL fks should violate the CHECK constraint.
    let err = sqlx::query(
        "INSERT INTO memory_code_anchor
            (entity_id, file_id, chunk_id, topic_id, anchor_type)
         VALUES ($1, NULL, NULL, NULL, 'implements')",
    )
    .bind(entity_id)
    .execute(pool)
    .await
    .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("check"),
        "expected CHECK constraint violation, got: {msg}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn create_entities_then_search_then_open_round_trip() {
    let db = require_test_db!();
    let pool = db.pool();
    let server = server_with_pool(pool.clone());

    // create_entities
    let create = server
        .call_tool_cli(
            "memory_create_entities",
            serde_json::json!({
                "entities": [
                    {
                        "name": "rust",
                        "entity_type": "language",
                        "observations": ["statically typed", "memory safe"]
                    },
                    {
                        "name": "ownership",
                        "entity_type": "concept",
                        "observations": ["borrow checker"]
                    }
                ]
            }),
        )
        .await
        .expect("memory_create_entities");
    let create_body = extract_json(&create);
    assert_eq!(
        create_body.get("entities_created").and_then(Value::as_i64),
        Some(2)
    );
    assert!(create_body.get("scope_id").and_then(Value::as_i64).unwrap() > 0);

    // search_nodes — should find both via 'rust' / 'ownership'.
    let search = server
        .call_tool_cli(
            "memory_search_nodes",
            serde_json::json!({"query": "borrow"}),
        )
        .await
        .expect("memory_search_nodes");
    let body = extract_json(&search);
    let count = body.get("count").and_then(Value::as_i64).unwrap_or(-1);
    assert!(count >= 1, "expected match for 'borrow', body={body}");

    // open_nodes for the two entities.
    let opened = server
        .call_tool_cli(
            "memory_open_nodes",
            serde_json::json!({"names": ["rust", "ownership"]}),
        )
        .await
        .expect("memory_open_nodes");
    let body = extract_json(&opened);
    let nodes = body
        .get("nodes")
        .and_then(Value::as_array)
        .expect("nodes array");
    assert_eq!(nodes.len(), 2);
    let names: Vec<_> = nodes
        .iter()
        .filter_map(|n| n.get("entity")?.get("name").and_then(Value::as_str))
        .collect();
    assert!(names.contains(&"rust"));
    assert!(names.contains(&"ownership"));

    // Observations attached at create-time should be present.
    for node in nodes {
        let observations = node
            .get("observations")
            .and_then(Value::as_array)
            .expect("observations array");
        assert!(
            !observations.is_empty(),
            "expected initial observations on opened node: {node}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn create_and_delete_relations_round_trip() {
    let db = require_test_db!();
    let pool = db.pool();
    let server = server_with_pool(pool.clone());

    // Seed two entities for the relation endpoints.
    server
        .call_tool_cli(
            "memory_create_entities",
            serde_json::json!({
                "entities": [
                    {"name": "alice", "entity_type": "person"},
                    {"name": "pgmcp", "entity_type": "project"}
                ]
            }),
        )
        .await
        .expect("seed entities");

    // create_relations
    let created = server
        .call_tool_cli(
            "memory_create_relations",
            serde_json::json!({
                "relations": [
                    {"from": "alice", "to": "pgmcp", "relation_type": "maintains"}
                ]
            }),
        )
        .await
        .expect("memory_create_relations");
    let body = extract_json(&created);
    assert_eq!(
        body.get("relations_created").and_then(Value::as_i64),
        Some(1)
    );

    // read_graph should surface the relation.
    let graph = server
        .call_tool_cli("memory_read_graph", serde_json::json!({}))
        .await
        .expect("memory_read_graph");
    let body = extract_json(&graph);
    let relations = body
        .get("relations")
        .and_then(Value::as_array)
        .expect("relations array");
    assert!(
        relations
            .iter()
            .any(|r| r.get("relation_type").and_then(Value::as_str) == Some("maintains")),
        "expected the 'maintains' relation in graph dump"
    );

    // delete_relations — soft delete.
    let deleted = server
        .call_tool_cli(
            "memory_delete_relations",
            serde_json::json!({
                "relations": [
                    {"from": "alice", "to": "pgmcp", "relation_type": "maintains"}
                ]
            }),
        )
        .await
        .expect("memory_delete_relations");
    let body = extract_json(&deleted);
    assert!(body.get("soft_deleted").and_then(Value::as_i64).unwrap() >= 1);

    // Re-read; the active filter should hide the relation.
    let graph2 = server
        .call_tool_cli("memory_read_graph", serde_json::json!({}))
        .await
        .expect("read after delete");
    let body = extract_json(&graph2);
    let relations = body
        .get("relations")
        .and_then(Value::as_array)
        .expect("relations array");
    assert!(
        relations
            .iter()
            .all(|r| r.get("relation_type").and_then(Value::as_str) != Some("maintains")),
        "deleted relation must not surface in active read_graph: {relations:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_entities_soft_deletes_via_valid_to() {
    let db = require_test_db!();
    let pool = db.pool();
    let server = server_with_pool(pool.clone());

    server
        .call_tool_cli(
            "memory_create_entities",
            serde_json::json!({
                "entities": [{"name": "doomed", "entity_type": "concept"}]
            }),
        )
        .await
        .expect("create");

    let res = server
        .call_tool_cli(
            "memory_delete_entities",
            serde_json::json!({"names": ["doomed"]}),
        )
        .await
        .expect("delete");
    let body = extract_json(&res);
    assert!(body.get("soft_deleted").and_then(Value::as_i64).unwrap() >= 1);
    assert_eq!(
        body.get("mode").and_then(Value::as_str),
        Some("soft_delete_via_valid_to")
    );

    // The row must still exist physically but with valid_to set.
    let count_active: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM memory_entities WHERE name = 'doomed' AND valid_to IS NULL",
    )
    .fetch_one(pool)
    .await
    .expect("count active");
    let count_total: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM memory_entities WHERE name = 'doomed'")
            .fetch_one(pool)
            .await
            .expect("count total");
    assert_eq!(count_active, 0);
    assert!(count_total >= 1, "soft delete must preserve the row");

    // open_nodes returns nothing for the soft-deleted name.
    let opened = server
        .call_tool_cli(
            "memory_open_nodes",
            serde_json::json!({"names": ["doomed"]}),
        )
        .await
        .expect("open after delete");
    let body = extract_json(&opened);
    let nodes = body
        .get("nodes")
        .and_then(Value::as_array)
        .expect("nodes array");
    assert!(nodes.is_empty(), "no active rows should remain");
}

#[tokio::test(flavor = "multi_thread")]
async fn create_entities_rejects_empty_input() {
    let db = require_test_db!();
    let pool = db.pool();
    let server = server_with_pool(pool.clone());
    let result = server
        .call_tool_cli(
            "memory_create_entities",
            serde_json::json!({"entities": []}),
        )
        .await;
    match result {
        Err(_) => {}
        Ok(r) => assert_eq!(
            r.is_error,
            Some(true),
            "expected error for empty entities, got {r:?}"
        ),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_observations_soft_deletes_targeted_content() {
    let db = require_test_db!();
    let pool = db.pool();
    let server = server_with_pool(pool.clone());

    server
        .call_tool_cli(
            "memory_create_entities",
            serde_json::json!({
                "entities": [
                    {"name": "del-obs", "entity_type": "concept",
                     "observations": ["alpha-content", "beta-content"]}
                ]
            }),
        )
        .await
        .expect("seed entity");

    let result = server
        .call_tool_cli(
            "memory_delete_observations",
            serde_json::json!({
                "deletions": [
                    {"entity_name": "del-obs", "observations": ["alpha-content"]}
                ]
            }),
        )
        .await
        .expect("memory_delete_observations");
    let body = extract_json(&result);
    assert_eq!(
        body.get("mode").and_then(Value::as_str),
        Some("soft_delete_via_valid_to")
    );
    assert!(body.get("soft_deleted").and_then(Value::as_i64).unwrap() >= 1);

    // Opening the entity should now show only the beta observation.
    let opened = server
        .call_tool_cli(
            "memory_open_nodes",
            serde_json::json!({"names": ["del-obs"]}),
        )
        .await
        .expect("open after delete");
    let body = extract_json(&opened);
    let nodes = body
        .get("nodes")
        .and_then(Value::as_array)
        .expect("nodes array");
    assert_eq!(nodes.len(), 1);
    let obs = nodes[0]
        .get("observations")
        .and_then(Value::as_array)
        .expect("observations array");
    let obs_texts: Vec<&str> = obs.iter().filter_map(Value::as_str).collect();
    assert!(obs_texts.contains(&"beta-content"), "beta should survive");
    assert!(
        !obs_texts.contains(&"alpha-content"),
        "alpha must be soft-deleted: {obs_texts:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn add_observations_dedupes_repeat_inserts() {
    let db = require_test_db!();
    let pool = db.pool();
    let server = server_with_pool(pool.clone());

    server
        .call_tool_cli(
            "memory_create_entities",
            serde_json::json!({
                "entities": [{"name": "dedupe-test", "entity_type": "concept"}]
            }),
        )
        .await
        .expect("create");

    let first = server
        .call_tool_cli(
            "memory_add_observations",
            serde_json::json!({
                "observations": [
                    {"entity_name": "dedupe-test", "contents": ["hello", "world"]}
                ]
            }),
        )
        .await
        .expect("first add");
    let body = extract_json(&first);
    assert_eq!(
        body.get("observations_added").and_then(Value::as_i64),
        Some(2)
    );

    // Same content again → 0 new rows.
    let second = server
        .call_tool_cli(
            "memory_add_observations",
            serde_json::json!({
                "observations": [
                    {"entity_name": "dedupe-test", "contents": ["hello", "world"]}
                ]
            }),
        )
        .await
        .expect("second add");
    let body = extract_json(&second);
    assert_eq!(
        body.get("observations_added").and_then(Value::as_i64),
        Some(0),
        "duplicate observations must dedupe"
    );
}
