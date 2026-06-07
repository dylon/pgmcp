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

use pgmcp::db::queries::{self, NewEntityInput, NewRelationInput, ScopeSpec};
use pgmcp_testing::pool_tool_helpers::server_with_pool;
use pgmcp_testing::require_test_db;
use serde_json::Value;
use uuid::Uuid;

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
async fn create_relations_normalizes_and_reports_actual_inserts() {
    let db = require_test_db!();
    let pool = db.pool();
    let server = server_with_pool(pool.clone());
    let from = format!("rel-from-{}", Uuid::new_v4().simple());
    let to = format!("rel-to-{}", Uuid::new_v4().simple());
    let relation_type = format!("maintains-{}", Uuid::new_v4().simple());

    server
        .call_tool_cli(
            "memory_create_entities",
            serde_json::json!({
                "entities": [
                    {"name": from.clone(), "entity_type": "person"},
                    {"name": to.clone(), "entity_type": "project"}
                ]
            }),
        )
        .await
        .expect("seed relation endpoints");

    let first = server
        .call_tool_cli(
            "memory_create_relations",
            serde_json::json!({
                "relations": [
                    {
                        "from": format!("  {from}  "),
                        "to": format!("  {to}  "),
                        "relation_type": format!("  {relation_type}  ")
                    },
                    {
                        "from": from.clone(),
                        "to": to.clone(),
                        "relation_type": relation_type.clone()
                    }
                ]
            }),
        )
        .await
        .expect("create relation twice in one request");
    let first_body = extract_json(&first);
    assert_eq!(
        first_body.get("relations_created").and_then(Value::as_i64),
        Some(1),
        "duplicate active relation should insert once"
    );
    assert_eq!(
        first_body.get("relations_resolved").and_then(Value::as_i64),
        Some(2),
        "both normalized inputs should resolve to an active relation id"
    );
    let ids = first_body
        .get("ids")
        .and_then(Value::as_array)
        .expect("ids array");
    assert_eq!(ids.len(), 2);
    assert_eq!(ids[0], ids[1], "duplicate inputs should reuse one row id");

    let second = server
        .call_tool_cli(
            "memory_create_relations",
            serde_json::json!({
                "relations": [
                    {
                        "from": from.clone(),
                        "to": to.clone(),
                        "relation_type": relation_type.clone()
                    }
                ]
            }),
        )
        .await
        .expect("idempotent relation create");
    let second_body = extract_json(&second);
    assert_eq!(
        second_body.get("relations_created").and_then(Value::as_i64),
        Some(0),
        "idempotent retry should not be counted as a new insert"
    );
    assert_eq!(
        second_body
            .get("relations_resolved")
            .and_then(Value::as_i64),
        Some(1)
    );

    let active_relations: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
         FROM memory_relations r
         JOIN memory_entities a ON a.id = r.from_entity_id
         JOIN memory_entities b ON b.id = r.to_entity_id
         WHERE a.name = $1
           AND b.name = $2
           AND r.relation_type = $3
           AND r.valid_to IS NULL",
    )
    .bind(&from)
    .bind(&to)
    .bind(&relation_type)
    .fetch_one(pool)
    .await
    .expect("count active relations");
    assert_eq!(active_relations, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn create_relations_rejects_blank_fields_before_write() {
    let db = require_test_db!();
    let pool = db.pool();
    let server = server_with_pool(pool.clone());
    let from = format!("blank-rel-from-{}", Uuid::new_v4().simple());
    let to = format!("blank-rel-to-{}", Uuid::new_v4().simple());

    server
        .call_tool_cli(
            "memory_create_entities",
            serde_json::json!({
                "entities": [
                    {"name": from.clone(), "entity_type": "person"},
                    {"name": to.clone(), "entity_type": "project"}
                ]
            }),
        )
        .await
        .expect("seed relation endpoints");

    let err = server
        .call_tool_cli(
            "memory_create_relations",
            serde_json::json!({
                "relations": [
                    {"from": from.clone(), "to": to.clone(), "relation_type": "   "}
                ]
            }),
        )
        .await
        .expect_err("blank relation_type must fail");
    assert!(
        err.to_string().contains("relation_type must not be blank"),
        "unexpected validation error: {err}"
    );

    let active_relations: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
         FROM memory_relations r
         JOIN memory_entities a ON a.id = r.from_entity_id
         JOIN memory_entities b ON b.id = r.to_entity_id
         WHERE a.name = $1
           AND b.name = $2
           AND r.valid_to IS NULL",
    )
    .bind(&from)
    .bind(&to)
    .fetch_one(pool)
    .await
    .expect("count active relations");
    assert_eq!(active_relations, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn create_relations_rejects_ambiguous_active_endpoint_name() {
    let db = require_test_db!();
    let pool = db.pool();
    let server = server_with_pool(pool.clone());
    let ambiguous = format!("ambiguous-rel-{}", Uuid::new_v4().simple());
    let target = format!("ambiguous-rel-target-{}", Uuid::new_v4().simple());

    sqlx::query(
        "INSERT INTO memory_entities (name, entity_type, source)
         VALUES
            ($1, 'person', 'agent_write'::memory_source),
            ($1, 'project', 'agent_write'::memory_source),
            ($2, 'concept', 'agent_write'::memory_source)",
    )
    .bind(&ambiguous)
    .bind(&target)
    .execute(pool)
    .await
    .expect("seed ambiguous endpoint name");

    let err = server
        .call_tool_cli(
            "memory_create_relations",
            serde_json::json!({
                "relations": [
                    {
                        "from": ambiguous.clone(),
                        "to": target.clone(),
                        "relation_type": "maintains"
                    }
                ]
            }),
        )
        .await
        .expect_err("ambiguous endpoint must fail closed");
    assert!(
        err.to_string()
            .contains("ambiguous memory relation endpoint name"),
        "unexpected ambiguous endpoint error: {err}"
    );

    let active_relations: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
         FROM memory_relations r
         JOIN memory_entities a ON a.id = r.from_entity_id
         JOIN memory_entities b ON b.id = r.to_entity_id
         WHERE a.name = $1
           AND b.name = $2
           AND r.valid_to IS NULL",
    )
    .bind(&ambiguous)
    .bind(&target)
    .fetch_one(pool)
    .await
    .expect("count active relations");
    assert_eq!(
        active_relations, 0,
        "ambiguous endpoint request must not create a relation"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_relations_concurrent_same_triple_is_single_active_row() {
    let db = require_test_db!();
    let pool = db.pool();
    let from = format!("concurrent-rel-from-{}", Uuid::new_v4().simple());
    let to = format!("concurrent-rel-to-{}", Uuid::new_v4().simple());
    let relation_type = "observes";

    sqlx::query(
        "INSERT INTO memory_entities (name, entity_type, source)
         VALUES
            ($1, 'person', 'agent_write'::memory_source),
            ($2, 'project', 'agent_write'::memory_source)",
    )
    .bind(&from)
    .bind(&to)
    .execute(pool)
    .await
    .expect("seed concurrent relation endpoints");

    let mut handles = Vec::new();
    for _ in 0..16 {
        let pool = pool.clone();
        let input = NewRelationInput {
            from: from.clone(),
            to: to.clone(),
            relation_type: relation_type.to_string(),
        };
        handles.push(tokio::spawn(async move {
            queries::memory_create_relations_detailed(&pool, &[input], "agent_write").await
        }));
    }

    let mut inserted = 0usize;
    let mut ids = Vec::new();
    for handle in handles {
        let result = handle
            .await
            .expect("join concurrent relation create")
            .expect("concurrent relation create");
        inserted += result.relations_inserted;
        ids.extend(result.relation_ids);
    }

    assert_eq!(
        inserted, 1,
        "concurrent creates should insert one active relation"
    );
    assert!(ids.iter().all(|id| *id >= 0));
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(
        ids.len(),
        1,
        "all concurrent callers should observe one relation id"
    );

    let active_relations: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
         FROM memory_relations r
         JOIN memory_entities a ON a.id = r.from_entity_id
         JOIN memory_entities b ON b.id = r.to_entity_id
         WHERE a.name = $1
           AND b.name = $2
           AND r.relation_type = $3
           AND r.valid_to IS NULL",
    )
    .bind(&from)
    .bind(&to)
    .bind(relation_type)
    .fetch_one(pool)
    .await
    .expect("count active concurrent relations");
    assert_eq!(active_relations, 1);
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
async fn create_entities_rejects_invalid_payload_before_scope_write() {
    let db = require_test_db!();
    let pool = db.pool();
    let server = server_with_pool(pool.clone());
    let user_id = format!("invalid-create-scope-{}", Uuid::new_v4().simple());

    let err = server
        .call_tool_cli(
            "memory_create_entities",
            serde_json::json!({
                "scope": {"user_id": user_id.clone()},
                "entities": [{"name": "   ", "entity_type": "concept"}]
            }),
        )
        .await
        .expect_err("blank entity name must fail");

    assert!(
        err.to_string().contains("entity name must not be blank"),
        "unexpected validation error: {err}"
    );

    let scope_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM memory_scope WHERE user_id = $1")
            .bind(&user_id)
            .fetch_one(pool)
            .await
            .expect("count memory_scope rows");
    assert_eq!(
        scope_count, 0,
        "invalid create request must not create a scope row"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn create_entities_normalizes_identity_and_reports_actual_inserts() {
    let db = require_test_db!();
    let pool = db.pool();
    let server = server_with_pool(pool.clone());
    let name = format!("normalized-create-{}", Uuid::new_v4().simple());

    let first = server
        .call_tool_cli(
            "memory_create_entities",
            serde_json::json!({
                "entities": [
                    {
                        "name": format!("  {name}  "),
                        "entity_type": "  concept  ",
                        "observations": ["deduped observation", "deduped observation"]
                    }
                ]
            }),
        )
        .await
        .expect("first create");
    let first_body = extract_json(&first);
    assert_eq!(
        first_body.get("entities_created").and_then(Value::as_i64),
        Some(1)
    );
    assert_eq!(
        first_body.get("entities_processed").and_then(Value::as_i64),
        Some(1)
    );
    assert_eq!(
        first_body
            .get("observations_attached")
            .and_then(Value::as_i64),
        Some(1),
        "duplicate observations in one create request should attach once"
    );

    let second = server
        .call_tool_cli(
            "memory_create_entities",
            serde_json::json!({
                "entities": [
                    {
                        "name": name.clone(),
                        "entity_type": "concept",
                        "observations": ["deduped observation"]
                    }
                ]
            }),
        )
        .await
        .expect("second create");
    let second_body = extract_json(&second);
    assert_eq!(
        second_body.get("entities_created").and_then(Value::as_i64),
        Some(0),
        "second create should reuse the active normalized identity"
    );
    assert_eq!(
        second_body
            .get("observations_attached")
            .and_then(Value::as_i64),
        Some(0),
        "duplicate active observations should not be reinserted"
    );

    let active_entities: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM memory_entities
         WHERE name = $1 AND entity_type = 'concept' AND valid_to IS NULL",
    )
    .bind(&name)
    .fetch_one(pool)
    .await
    .expect("count active normalized entities");
    assert_eq!(active_entities, 1);

    let active_observations: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
         FROM memory_observations o
         JOIN memory_entities e ON e.id = o.entity_id
         WHERE e.name = $1
           AND e.entity_type = 'concept'
           AND e.valid_to IS NULL
           AND o.valid_to IS NULL",
    )
    .bind(&name)
    .fetch_one(pool)
    .await
    .expect("count active normalized observations");
    assert_eq!(active_observations, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn create_entities_rejects_preexisting_duplicate_active_identity() {
    let db = require_test_db!();
    let pool = db.pool();
    let server = server_with_pool(pool.clone());
    let name = format!("duplicate-active-create-{}", Uuid::new_v4().simple());

    sqlx::query(
        "INSERT INTO memory_entities (name, entity_type, source, valid_from)
         VALUES
            ($1, 'concept', 'agent_write'::memory_source, NOW() - INTERVAL '1 second'),
            ($1, 'concept', 'agent_write'::memory_source, NOW())",
    )
    .bind(&name)
    .execute(pool)
    .await
    .expect("seed duplicate active identities");

    let err = server
        .call_tool_cli(
            "memory_create_entities",
            serde_json::json!({
                "entities": [
                    {"name": name, "entity_type": "concept", "observations": ["must not attach"]}
                ]
            }),
        )
        .await
        .expect_err("duplicate active identity must fail closed");

    assert!(
        err.to_string()
            .contains("ambiguous active memory entity identity"),
        "unexpected duplicate identity error: {err}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_entities_concurrent_same_identity_is_single_active_row() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let scope_id = queries::find_or_create_scope(&pool, &ScopeSpec::default())
        .await
        .expect("scope");
    let name = format!("concurrent-create-{}", Uuid::new_v4().simple());

    let mut handles = Vec::new();
    for i in 0..16 {
        let pool = pool.clone();
        let name = name.clone();
        handles.push(tokio::spawn(async move {
            let inputs = vec![NewEntityInput {
                name,
                entity_type: "concept".to_string(),
                observations: vec![format!("observation-{i}")],
            }];
            queries::memory_create_entities_detailed(&pool, &inputs, scope_id, "agent_write").await
        }));
    }

    let mut entity_ids = Vec::new();
    let mut inserted_entities = 0usize;
    let mut inserted_observations = 0usize;
    for handle in handles {
        let result = handle
            .await
            .expect("join concurrent create")
            .expect("create entity");
        entity_ids.extend(result.entity_ids);
        inserted_entities += result.entities_inserted;
        inserted_observations += result.observations_inserted;
    }

    assert_eq!(
        inserted_entities, 1,
        "concurrent creates should insert exactly one active entity"
    );
    assert_eq!(
        inserted_observations, 16,
        "distinct observations should all attach once"
    );
    let first_id = *entity_ids.first().expect("at least one returned entity id");
    assert!(
        entity_ids.iter().all(|id| *id == first_id),
        "all concurrent callers should observe the same entity id: {entity_ids:?}"
    );

    let active_entities: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM memory_entities
         WHERE name = $1 AND entity_type = 'concept' AND valid_to IS NULL",
    )
    .bind(&name)
    .fetch_one(&pool)
    .await
    .expect("count active race entities");
    assert_eq!(active_entities, 1);

    let active_observations: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
         FROM memory_observations o
         JOIN memory_entities e ON e.id = o.entity_id
         WHERE e.name = $1
           AND e.entity_type = 'concept'
           AND e.valid_to IS NULL
           AND o.valid_to IS NULL",
    )
    .bind(&name)
    .fetch_one(&pool)
    .await
    .expect("count active race observations");
    assert_eq!(active_observations, 16);
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

#[tokio::test(flavor = "multi_thread")]
async fn add_observations_rejects_ambiguous_active_entity_name() {
    let db = require_test_db!();
    let pool = db.pool();
    let server = server_with_pool(pool.clone());
    let name = format!("ambiguous-add-{}", Uuid::new_v4().simple());

    server
        .call_tool_cli(
            "memory_create_entities",
            serde_json::json!({
                "entities": [
                    {"name": name, "entity_type": "concept"},
                    {"name": name, "entity_type": "person"}
                ]
            }),
        )
        .await
        .expect("create ambiguous entities");

    let err = server
        .call_tool_cli(
            "memory_add_observations",
            serde_json::json!({
                "observations": [
                    {"entity_name": name, "contents": ["must not attach arbitrarily"]}
                ]
            }),
        )
        .await
        .expect_err("ambiguous entity name must fail closed");

    assert!(
        err.to_string().contains("ambiguous memory entity name"),
        "unexpected ambiguity error: {err}"
    );
}
