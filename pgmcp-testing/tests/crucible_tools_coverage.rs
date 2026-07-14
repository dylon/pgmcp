//! Coverage + smoke test for the a2a/csm tools merged from `crucible-pgmcp`
//! (a2a_fleet_view / orchestrator_recommend_next / csm_synthesize_protocol),
//! which arrived without integration tests. Drives each through call_tool_cli
//! (Layer-D coverage gate) and asserts the dispatch path executes.

use crate::common::{server_with_pool, text_of};
use pgmcp_testing::require_test_db;
use serde_json::json;

#[tokio::test]
async fn crucible_a2a_csm_tools_dispatch() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let server = server_with_pool(pool.clone());

    // a2a_fleet_view: lists fleet members (empty on a fresh DB) → Ok.
    let fleet = server
        .call_tool_cli("a2a_fleet_view", json!({"limit": 50}))
        .await
        .expect("a2a_fleet_view must dispatch");
    assert!(!text_of(&fleet).is_empty());

    // orchestrator_recommend_next: requires a non-empty specialty; an empty
    // fleet yields a graceful (empty) recommendation, not an error.
    let rec = server
        .call_tool_cli(
            "orchestrator_recommend_next",
            json!({"task": "port a module", "specialty": ["rust"]}),
        )
        .await
        .expect("orchestrator_recommend_next must dispatch");
    assert!(!text_of(&rec).is_empty());

    // csm_synthesize_protocol: fold a (minimal) plan subtree into a protocol.
    sqlx::query(
        "INSERT INTO work_items (public_id, kind, status, title)
         VALUES ('cov-csm-plan', 'task', 'pending', 'coverage plan')
         ON CONFLICT (public_id) DO NOTHING",
    )
    .execute(&pool)
    .await
    .expect("seed plan");
    let proto = server
        .call_tool_cli(
            "csm_synthesize_protocol",
            json!({"public_id": "cov-csm-plan"}),
        )
        .await
        .expect("csm_synthesize_protocol must dispatch");
    assert!(!text_of(&proto).is_empty());
}
