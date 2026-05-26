//! End-to-end test for the Part A cross-agent best-practice exchange:
//! two distinct agents independently report the same approach → the
//! consensus gate (≥2 agents) admits it → reflection promotes it to a
//! durable mandate that re-injects into every future agent turn.
//!
//! Drives everything through the public MCP dispatch path
//! (`a2a_report_outcome` + `trigger_cron job="a2a-reflect"`), no LLM, no
//! network — exactly the heuristic-first path that works with
//! `[memory] backend = "disabled"`.

use pgmcp_testing::pool_tool_helpers::{seed_project, server_with_pool};
use pgmcp_testing::require_test_db;

#[tokio::test(flavor = "multi_thread")]
async fn two_agents_agree_then_promote_to_durable_mandate() {
    let db = require_test_db!();
    let proj = seed_project(db.pool(), "a2a-bp-e2e", "/ws/a2a-bp-e2e").await;
    let server = server_with_pool(db.pool().clone());

    // Two distinct agents independently report the same winning approach.
    for agent in ["agent-a", "agent-b"] {
        let r = server
            .call_tool_cli(
                "a2a_report_outcome",
                serde_json::json!({
                    "task_kind": "rust-collections",
                    "approach": "preallocate Vec with capacity",
                    "outcome": "worked",
                    "confidence": 1.0,
                    "project_id": proj,
                    "agent_id": agent,
                }),
            )
            .await
            .expect("report_outcome");
        assert!(r.is_error != Some(true), "report from {agent} ok");
    }

    // Both reports land in the authoritative ledger.
    let outcomes: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_outcomes WHERE task_kind = 'rust-collections'",
    )
    .fetch_one(db.pool())
    .await
    .expect("count outcomes");
    assert_eq!(outcomes, 2, "two distinct-agent reports recorded");

    // Each report is mirrored into a best_practice memory entity, sha-deduped
    // and tier-tagged procedural — so the shared entity is reused across agents.
    let bp_entities: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM memory_entities
         WHERE entity_type = 'best_practice' AND valid_to IS NULL",
    )
    .fetch_one(db.pool())
    .await
    .expect("count entities");
    assert!(
        bp_entities >= 1,
        "approach captured as a best_practice entity"
    );

    // Run consensus reflection + promotion (heuristic; no LLM extractor).
    let r = server
        .call_tool_cli("trigger_cron", serde_json::json!({"job": "a2a-reflect"}))
        .await
        .expect("a2a-reflect");
    assert!(r.is_error != Some(true), "a2a-reflect ran");

    // The ≥2-agent-agreed practice was promoted to a durable mandate, which
    // re-injects via the UserPromptSubmit hook for every future agent turn.
    let promoted: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM durable_mandates WHERE imperative ILIKE '%preallocate%'",
    )
    .fetch_one(db.pool())
    .await
    .expect("count durable");
    assert!(
        promoted >= 1,
        "agreed best practice promoted to durable_mandates"
    );

    // The consensus practice is now attached to a shared (agent_id IS NULL)
    // scope — reachable by any future agent regardless of which agent first
    // reported it (G1 task-decoupled sharing).
    let shared: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM memory_entity_scope mes
         JOIN memory_scope s ON s.id = mes.scope_id
         JOIN memory_entities e ON e.id = mes.entity_id
         WHERE s.agent_id IS NULL AND e.entity_type = 'best_practice'",
    )
    .fetch_one(db.pool())
    .await
    .expect("count shared");
    assert!(shared >= 1, "agreed practice attached to a shared scope");
}
