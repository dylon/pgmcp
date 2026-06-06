//! Focused oracle coverage for `memory_open_nodes`.

use pgmcp_testing::pool_tool_helpers::server_with_pool;
use pgmcp_testing::require_test_db;
use serde_json::{Value, json};

fn text_of(result: &rmcp::model::CallToolResult) -> &str {
    for content in &result.content {
        if let rmcp::model::RawContent::Text(text) = &content.raw {
            return &text.text;
        }
    }
    panic!("tool returned no text content");
}

async fn insert_entity(pool: &sqlx::PgPool, name: &str, active: bool) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO memory_entities (name, entity_type, source, valid_to)
         VALUES ($1, 'concept', 'agent_write'::memory_source,
                 CASE WHEN $2::bool THEN NULL ELSE now() END)
         RETURNING id",
    )
    .bind(name)
    .bind(active)
    .fetch_one(pool)
    .await
    .expect("insert memory entity")
}

async fn insert_relation(pool: &sqlx::PgPool, from_entity_id: i64, to_entity_id: i64) {
    sqlx::query(
        "INSERT INTO memory_relations
            (from_entity_id, to_entity_id, relation_type, source)
         VALUES ($1, $2, 'related_to', 'agent_write'::memory_source)",
    )
    .bind(from_entity_id)
    .bind(to_entity_id)
    .execute(pool)
    .await
    .expect("insert memory relation");
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_open_nodes_normalizes_names_and_hides_inactive_relation_endpoints() {
    let db = require_test_db!();
    let pool = db.pool();

    let active = insert_entity(pool, "open-active", true).await;
    let inactive_out = insert_entity(pool, "open-deleted-out", false).await;
    let inactive_in = insert_entity(pool, "open-deleted-in", false).await;
    let active_neighbor = insert_entity(pool, "open-neighbor", true).await;

    insert_relation(pool, active, inactive_out).await;
    insert_relation(pool, inactive_in, active).await;
    insert_relation(pool, active, active_neighbor).await;

    let server = server_with_pool(pool.clone());
    let result = server
        .call_tool_cli(
            "memory_open_nodes",
            json!({"names": [" open-active ", "open-active"]}),
        )
        .await
        .expect("memory_open_nodes");
    let v: Value = serde_json::from_str(text_of(&result)).expect("json");
    assert_eq!(v["requested_names"], json!(["open-active"]));
    assert_eq!(v["name_cap"].as_u64(), Some(100));
    assert_eq!(v["count"].as_u64(), Some(1), "{v:#}");

    let node = &v["nodes"][0];
    let rel_out = node["relations_out"].as_array().expect("relations_out");
    assert_eq!(rel_out.len(), 1, "{v:#}");
    assert_eq!(rel_out[0]["to"].as_str(), Some("open-neighbor"));
    assert!(
        node["relations_in"]
            .as_array()
            .expect("relations_in")
            .is_empty(),
        "inactive incoming endpoint leaked: {v:#}"
    );

    let body = v.to_string();
    assert!(!body.contains("open-deleted-out"), "{v:#}");
    assert!(!body.contains("open-deleted-in"), "{v:#}");
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_open_nodes_rejects_blank_and_oversized_names() {
    let db = require_test_db!();
    let server = server_with_pool(db.pool().clone());

    assert!(
        server
            .call_tool_cli("memory_open_nodes", json!({"names": ["   "]}))
            .await
            .is_err(),
        "blank names must fail closed"
    );

    let too_many: Vec<String> = (0..101).map(|idx| format!("name-{idx}")).collect();
    assert!(
        server
            .call_tool_cli("memory_open_nodes", json!({"names": too_many}))
            .await
            .is_err(),
        "oversized name lists must fail closed"
    );
}
