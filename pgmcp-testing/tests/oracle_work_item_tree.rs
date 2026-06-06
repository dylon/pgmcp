//! Focused oracle coverage for `work_item_tree`.

mod common;

use std::collections::HashSet;

use common::text_of;
use pgmcp_testing::pool_tool_helpers::server_with_pool;
use pgmcp_testing::require_test_db;
use serde_json::{Value, json};
use uuid::Uuid;

async fn create_item(
    server: &pgmcp::mcp::server::McpServer,
    public_id: &str,
    title: &str,
    parent_public_id: Option<&str>,
    priority: i64,
) {
    let mut body = json!({
        "kind": "task",
        "title": title,
        "public_id": public_id,
        "priority": priority,
    });
    if let Some(parent) = parent_public_id {
        body["parent_public_id"] = json!(parent);
    }
    server
        .call_tool_cli("work_item_create", body)
        .await
        .expect("create work item");
}

async fn item_id(pool: &sqlx::PgPool, public_id: &str) -> i64 {
    sqlx::query_scalar("SELECT id FROM work_items WHERE public_id = $1")
        .bind(public_id)
        .fetch_one(pool)
        .await
        .expect("work item id")
}

#[tokio::test]
async fn work_item_tree_respects_limit_and_depth_priority_order() {
    let db = require_test_db!();
    let pool = db.pool();
    let server = server_with_pool(pool.clone());
    let suffix = Uuid::new_v4().simple();
    let root = format!("tree-root-{suffix}");
    let low = format!("tree-low-{suffix}");
    let high = format!("tree-high-{suffix}");

    create_item(&server, &root, "tree root", None, 1).await;
    create_item(&server, &low, "low priority child", Some(&root), 1).await;
    create_item(&server, &high, "high priority child", Some(&root), 9).await;

    let result = server
        .call_tool_cli(
            "work_item_tree",
            json!({ "public_id": root, "max_rows": 2 }),
        )
        .await
        .expect("tree");
    let rows: Value = serde_json::from_str(&text_of(&result)).expect("tree json");
    let rows = rows.as_array().expect("tree rows");
    assert_eq!(rows.len(), 2, "max_rows=2 returns root plus one child");
    assert_eq!(rows[0]["public_id"].as_str(), Some(root.as_str()));
    assert_eq!(
        rows[1]["public_id"].as_str(),
        Some(high.as_str()),
        "children at the same depth are priority-desc ordered before id"
    );
}

#[tokio::test]
async fn work_item_tree_suppresses_corrupt_parent_cycles() {
    let db = require_test_db!();
    let pool = db.pool();
    let server = server_with_pool(pool.clone());
    let suffix = Uuid::new_v4().simple();
    let root = format!("tree-cycle-root-{suffix}");
    let child = format!("tree-cycle-child-{suffix}");

    create_item(&server, &root, "cycle root", None, 1).await;
    create_item(&server, &child, "cycle child", Some(&root), 1).await;

    let root_id = item_id(pool, &root).await;
    let child_id = item_id(pool, &child).await;
    sqlx::query("UPDATE work_items SET parent_id = $1 WHERE id = $2")
        .bind(child_id)
        .bind(root_id)
        .execute(pool)
        .await
        .expect("create corrupt cycle");

    let result = server
        .call_tool_cli(
            "work_item_tree",
            json!({ "public_id": root, "max_rows": 10 }),
        )
        .await
        .expect("tree with corrupt cycle");
    let rows: Value = serde_json::from_str(&text_of(&result)).expect("tree json");
    let rows = rows.as_array().expect("tree rows");
    assert_eq!(
        rows.len(),
        2,
        "cycle suppression returns root and child exactly once"
    );

    let ids = rows
        .iter()
        .map(|row| row["id"].as_i64().expect("row id"))
        .collect::<Vec<_>>();
    let unique = ids.iter().copied().collect::<HashSet<_>>();
    assert_eq!(
        unique.len(),
        ids.len(),
        "tree rows do not repeat cycle nodes"
    );
}
