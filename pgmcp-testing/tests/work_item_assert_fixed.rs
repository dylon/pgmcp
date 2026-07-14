//! Integration test for `work_item_assert_fixed` (ADR-023, item 7): an agent
//! asserting a bug fixed freezes a machine-checkable criterion and advances the
//! bug to `claimed_done`, but CANNOT reach `verified` — the trust boundary holds.

use crate::common::{server_with_pool, text_of};
use pgmcp_testing::require_test_db;
use serde_json::json;

fn body(r: &rmcp::model::CallToolResult) -> serde_json::Value {
    serde_json::from_str(&text_of(r)).expect("tool body must be JSON")
}

#[tokio::test]
async fn assert_fixed_freezes_criterion_and_claims_done_not_verified() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let server = server_with_pool(pool.clone());

    // Seed an in-progress bug.
    sqlx::query(
        "INSERT INTO work_items (public_id, kind, status, title, severity)
         VALUES ('assertfix-bug-zzz', 'bug', 'in_progress', 'widget crashes', 'high')
         ON CONFLICT (public_id) DO UPDATE SET status = 'in_progress'",
    )
    .execute(&pool)
    .await
    .expect("seed bug");

    let res = body(
        &server
            .call_tool_cli(
                "work_item_assert_fixed",
                json!({
                    "public_id": "assertfix-bug-zzz",
                    "verification_command": "cargo test crash_repro_zzz",
                    "expected_signal": "test result: ok"
                }),
            )
            .await
            .expect("work_item_assert_fixed"),
    );
    assert_eq!(res["criterion_frozen"], true);
    assert_eq!(
        res["verified"], false,
        "an agent must NOT be able to self-verify a bug"
    );
    assert_eq!(res["advanced_to_claimed_done"], true);
    assert_eq!(
        res["status"], "claimed_done",
        "from in_progress the agent-legal step is claimed_done, not verified"
    );

    // The frozen criterion is persisted with a lock timestamp.
    let row: (Option<String>, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(
        "SELECT d.verification_command, d.criterion_locked_at
           FROM work_item_bug_details d JOIN work_items w ON w.id = d.item_id
          WHERE w.public_id = 'assertfix-bug-zzz'",
    )
    .fetch_one(&pool)
    .await
    .expect("bug details row");
    assert_eq!(row.0.as_deref(), Some("cargo test crash_repro_zzz"));
    assert!(row.1.is_some(), "criterion must be frozen (locked_at set)");

    // The bug is NOT verified in the DB.
    let status: String =
        sqlx::query_scalar("SELECT status FROM work_items WHERE public_id = 'assertfix-bug-zzz'")
            .fetch_one(&pool)
            .await
            .expect("status");
    assert_ne!(
        status, "verified",
        "the bug must not be verified by the agent"
    );

    // A non-bug is rejected.
    sqlx::query(
        "INSERT INTO work_items (public_id, kind, status, title)
         VALUES ('assertfix-task-zzz', 'task', 'in_progress', 'not a bug')
         ON CONFLICT (public_id) DO NOTHING",
    )
    .execute(&pool)
    .await
    .expect("seed task");
    assert!(
        server
            .call_tool_cli(
                "work_item_assert_fixed",
                json!({"public_id": "assertfix-task-zzz", "verification_command": "x"})
            )
            .await
            .is_err(),
        "work_item_assert_fixed must reject a non-bug kind"
    );
}
