//! Focused oracle coverage for `work_item_claim`.

use crate::common::text_of;
use pgmcp_testing::pool_tool_helpers::server_with_pool;
use pgmcp_testing::require_test_db;
use serde_json::{Value, json};
use uuid::Uuid;

async fn create_claimable_item(
    server: &pgmcp::mcp::server::McpServer,
    public_id: &str,
    title: &str,
) {
    server
        .call_tool_cli(
            "work_item_create",
            json!({ "kind": "task", "title": title, "public_id": public_id }),
        )
        .await
        .expect("create work item");
}

async fn claim_state(pool: &sqlx::PgPool, public_id: &str) -> (Option<String>, i32) {
    sqlx::query_as("SELECT claimed_by, claim_count FROM work_items WHERE public_id = $1")
        .bind(public_id)
        .fetch_one(pool)
        .await
        .expect("claim state")
}

async fn claim_ledger_count(pool: &sqlx::PgPool, public_id: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*)
           FROM work_item_claims c
           JOIN work_items w ON w.id = c.work_item_id
          WHERE w.public_id = $1",
    )
    .bind(public_id)
    .fetch_one(pool)
    .await
    .expect("claim ledger count")
}

#[tokio::test]
async fn work_item_claim_rejects_blank_agent_without_side_effects() {
    let db = require_test_db!();
    let pool = db.pool();
    let server = server_with_pool(pool.clone());
    let public_id = format!("claim-blank-{}", Uuid::new_v4().simple());
    create_claimable_item(&server, &public_id, "blank agent claim").await;

    assert!(
        server
            .call_tool_cli(
                "work_item_claim",
                json!({ "public_id": public_id, "agent_id": "   " }),
            )
            .await
            .is_err(),
        "blank explicit agent_id must fail closed"
    );

    let (owner, claim_count) = claim_state(pool, &public_id).await;
    assert_eq!(owner, None);
    assert_eq!(claim_count, 0);
    assert_eq!(claim_ledger_count(pool, &public_id).await, 0);
}

#[tokio::test]
async fn work_item_claim_trims_agent_before_claim_and_presence() {
    let db = require_test_db!();
    let pool = db.pool();
    let server = server_with_pool(pool.clone());
    let public_id = format!("claim-trim-{}", Uuid::new_v4().simple());
    create_claimable_item(&server, &public_id, "trimmed agent claim").await;

    let result = server
        .call_tool_cli(
            "work_item_claim",
            json!({ "public_id": public_id, "agent_id": "  agent-trim  " }),
        )
        .await
        .expect("claim with trimmed agent");
    let body: Value = serde_json::from_str(&text_of(&result)).expect("claim json");
    assert_eq!(body["claimed"].as_bool(), Some(true));
    assert_eq!(body["by"].as_str(), Some("agent-trim"));

    let (owner, claim_count) = claim_state(pool, &public_id).await;
    assert_eq!(owner.as_deref(), Some("agent-trim"));
    assert_eq!(claim_count, 1);

    let ledger_agent: String = sqlx::query_scalar(
        "SELECT c.agent_id
           FROM work_item_claims c
           JOIN work_items w ON w.id = c.work_item_id
          WHERE w.public_id = $1
          ORDER BY c.created_at DESC
          LIMIT 1",
    )
    .bind(&public_id)
    .fetch_one(pool)
    .await
    .expect("claim ledger agent");
    assert_eq!(ledger_agent, "agent-trim");

    let presence_exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM agent_presence WHERE agent_id = $1)")
            .bind("agent-trim")
            .fetch_one(pool)
            .await
            .expect("agent presence");
    assert!(presence_exists);
}
