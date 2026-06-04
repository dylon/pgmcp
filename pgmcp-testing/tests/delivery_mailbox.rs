//! Real-Postgres test for Phase-3 message delivery (`a2a::delivery`): a
//! project-addressed message renders into the "📨 Agent messages" block on first
//! delivery to a session, is receipt-deduped on re-delivery to that session, and
//! is delivered independently to a different session (per-session dedup).

use pgmcp::a2a::delivery::render_and_deliver;
use pgmcp::a2a::mailbox_store::{self, NewMessage};
use pgmcp_testing::pool_tool_helpers::seed_project;
use pgmcp_testing::require_test_db;

#[tokio::test(flavor = "multi_thread")]
async fn delivery_renders_and_dedups_per_session() {
    let db = require_test_db!();
    let pool = db.pool();
    let project = seed_project(pool, "deliv-proj", "/ws/deliv-proj").await;

    let msg = NewMessage {
        from_agent: "claude-code",
        from_session: Some("sender"),
        to_session: None,
        to_project_id: Some(project),
        to_agent: None,
        kind: "request",
        subject: Some("heads up"),
        body: "a dependency is changing",
        reply_to: None,
        expires_at: None,
    };
    let _id = mailbox_store::send(pool, &msg).await.expect("send");

    // First delivery to session S1 renders the block.
    let b1 = render_and_deliver(
        pool,
        Some("S1"),
        Some(project),
        Some("claude-code"),
        "prompt",
        5,
    )
    .await;
    assert!(
        b1.as_deref().is_some_and(|b| b.contains("Agent messages")),
        "S1 first delivery should render: {b1:?}"
    );

    // Re-delivery to S1 is deduped (already delivered).
    let b1b = render_and_deliver(
        pool,
        Some("S1"),
        Some(project),
        Some("claude-code"),
        "prompt",
        5,
    )
    .await;
    assert!(b1b.is_none(), "S1 re-delivery should be deduped: {b1b:?}");

    // A different session S2 sees it independently (per-session receipts).
    let b2 = render_and_deliver(
        pool,
        Some("S2"),
        Some(project),
        Some("claude-code"),
        "prompt",
        5,
    )
    .await;
    assert!(
        b2.is_some(),
        "S2 should get an independent delivery: {b2:?}"
    );
}
