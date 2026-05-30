//! Integration smoke test for the Phase-3 git/PR close-the-loop surface.
//!
//! Two halves, both against real Postgres (self-skips via `require_test_db!`
//! when `PGMCP_TEST_DATABASE_URL` is unset):
//!
//! 1. The `work_item_link_commit` MCP tool — create an item, link a commit
//!    (link_type inferred from the SHA shape), then RE-link the same ref and
//!    assert it is idempotent (`created=false`, the UNIQUE(item,type,ref) row is
//!    upserted, not duplicated). This satisfies the `query_inventory_vs_coverage`
//!    gate, which greps for `call_tool_cli("work_item_link_commit", …)`.
//!
//! 2. The "a merge NEVER verifies" trust regression — drive the exact transition
//!    arcs the REST `pr_event` merge path performs (`Actor::Agent`:
//!    Pending→InProgress→Verifying), assert the item reaches `verifying` (a
//!    verify *candidate*), then assert a no-evidence `Actor::Gatekeeper` verify
//!    is STILL refused. This is the crux of the Phase-3 trust boundary: an
//!    agent-grade signal (a merge) can never reach `verified`.

mod common;

use std::sync::Arc;

use arc_swap::ArcSwap;
use common::text_of;
use pgmcp::config::Config;
use pgmcp::context::SystemContext;
use pgmcp::db::DbClient;
use pgmcp::db::queries::{self, NewWorkItem};
use pgmcp::embed::EmbedSource;
use pgmcp::mcp::logging::LogBroadcaster;
use pgmcp::mcp::server::McpServer;
use pgmcp::mcp::tasks::TaskStore;
use pgmcp::stats::tracker::StatsTracker;
use pgmcp::tracker::status::WorkItemStatus;
use pgmcp::tracker::transition::{Actor, TransitionError};
use pgmcp_testing::mocks::DeterministicEmbeddingBackend;
use pgmcp_testing::require_test_db;
use serde_json::{Value, json};
use sqlx::PgPool;

fn server_1024(pool: PgPool) -> McpServer {
    let db: Arc<dyn DbClient> = Arc::new(pool);
    let stats = Arc::new(StatsTracker::new());
    let config = Arc::new(ArcSwap::from_pointee(Config::default()));
    let log_broadcaster = Arc::new(LogBroadcaster::new());
    let task_store = Arc::new(TaskStore::new());
    let embed_backend: Arc<dyn pgmcp::embed::EmbeddingBackend> =
        Arc::new(DeterministicEmbeddingBackend::new(1024));
    let embed_source = EmbedSource::backend(embed_backend);
    let lifecycle = pgmcp::daemon_state::DaemonLifecycle::new();
    lifecycle.transition(pgmcp::daemon_state::DaemonPhase::Ready);
    let ctx = SystemContext::production(
        db,
        embed_source,
        stats,
        config,
        log_broadcaster,
        task_store,
        lifecycle,
    );
    McpServer::new(ctx)
}

fn public_id_of(v: &Value) -> String {
    v["public_id"]
        .as_str()
        .expect("row carries a public_id")
        .to_string()
}

#[tokio::test]
async fn work_item_link_commit_is_idempotent() {
    let db = require_test_db!();
    let server = server_1024(db.pool().clone());

    // Create a plain task to link.
    let created = server
        .call_tool_cli(
            "work_item_create",
            json!({ "kind": "task", "title": "wire the git loop" }),
        )
        .await
        .expect("work_item_create must succeed");
    let cv: Value = serde_json::from_str(&text_of(&created)).expect("create body JSON");
    let public_id = public_id_of(&cv);

    // Link a commit SHA — link_type omitted, inferred from the hex shape.
    let sha = "0f3e647a1b2c3d4e5f60718293a4b5c6d7e8f900";
    let linked = server
        .call_tool_cli(
            "work_item_link_commit",
            json!({ "public_id": public_id, "ref_value": sha }),
        )
        .await
        .expect("work_item_link_commit must succeed");
    let lv: Value = serde_json::from_str(&text_of(&linked)).expect("link body JSON");
    assert_eq!(lv["created"].as_bool(), Some(true), "first link is created");
    assert_eq!(
        lv["link_type"].as_str(),
        Some("commit"),
        "an all-hex ref ≥ 7 chars infers link_type=commit"
    );
    assert_eq!(lv["ref_value"].as_str(), Some(sha));

    // Re-link the SAME (item, type, ref) — idempotent: created=false, no dup row.
    let relinked = server
        .call_tool_cli(
            "work_item_link_commit",
            json!({ "public_id": public_id, "ref_value": sha, "link_type": "commit" }),
        )
        .await
        .expect("re-link must succeed");
    let rv: Value = serde_json::from_str(&text_of(&relinked)).expect("relink body JSON");
    assert_eq!(
        rv["created"].as_bool(),
        Some(false),
        "re-linking the same ref is idempotent (no new row)"
    );
    assert_eq!(
        rv["link_id"].as_i64(),
        lv["link_id"].as_i64(),
        "the upsert returns the same row id"
    );

    // Exactly one git-link row for this item exists.
    let link_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM work_item_git_links l \
         JOIN work_items w ON w.id = l.item_id WHERE w.public_id = $1",
    )
    .bind(&public_id)
    .fetch_one(db.pool())
    .await
    .expect("count git links");
    assert_eq!(link_count, 1, "re-link did not create a duplicate row");

    // A PR-number ref infers link_type=pr.
    let pr_link = server
        .call_tool_cli(
            "work_item_link_commit",
            json!({ "public_id": public_id, "ref_value": "#4567" }),
        )
        .await
        .expect("pr link must succeed");
    let pv: Value = serde_json::from_str(&text_of(&pr_link)).expect("pr link body JSON");
    assert_eq!(
        pv["link_type"].as_str(),
        Some("pr"),
        "a #<digits> ref infers link_type=pr"
    );
}

/// THE TRUST REGRESSION: a merge advances at most to `verifying` and can NEVER
/// reach `verified`. We drive the precise arcs the REST `pr_event` merge path
/// uses (`advance_agent_to_verifying`: a sequence of `Actor::Agent`
/// `set_work_item_status` calls), then assert a no-evidence gatekeeper verify is
/// still refused — exactly as the live system behaves.
#[tokio::test]
async fn pr_merge_advances_to_verifying_but_never_verified() {
    let db = require_test_db!();
    let pool = db.pool().clone();

    // Seed a pending item (workspace-global; project_id NULL is fine).
    let item_id = queries::insert_work_item(
        &pool,
        NewWorkItem {
            public_id: "git-merge-trust-aa11bb",
            kind: "task",
            status: "pending",
            title: "verify the merge trust boundary",
            ..Default::default()
        },
    )
    .await
    .expect("seed item");

    // ── what pr_event's merge does: Actor::Agent Pending→InProgress→Verifying ──
    let r1 = queries::set_work_item_status(
        &pool,
        item_id,
        WorkItemStatus::InProgress,
        Actor::Agent,
        Some("pr-webhook"),
        Some("git: PR merged (verify candidate)"),
        None,
        None,
    )
    .await
    .expect("agent pending→in_progress is legal");
    assert_eq!(r1.status, "in_progress");

    let r2 = queries::set_work_item_status(
        &pool,
        item_id,
        WorkItemStatus::Verifying,
        Actor::Agent,
        Some("pr-webhook"),
        Some("git: PR merged (verify candidate)"),
        None,
        None,
    )
    .await
    .expect("agent in_progress→verifying is legal");
    assert_eq!(
        r2.status, "verifying",
        "a merge advances the item to a verify CANDIDATE (verifying)"
    );

    // ── the crux: Actor::Agent can NEVER reach verified (no agent arm) ──
    let agent_verify = queries::set_work_item_status(
        &pool,
        item_id,
        WorkItemStatus::Verified,
        Actor::Agent,
        Some("pr-webhook"),
        Some("attempt"),
        None,
        None,
    )
    .await;
    assert!(
        agent_verify.is_err(),
        "an agent (the merge path's actor) can NEVER reach verified"
    );

    // ── and a gatekeeper verify with NO passing evidence is STILL refused ──
    let gatekeeper_no_evidence = queries::set_work_item_status(
        &pool,
        item_id,
        WorkItemStatus::Verified,
        Actor::Gatekeeper,
        Some("ci"),
        Some("attempt without evidence"),
        None, // no evidence_id
        None,
    )
    .await;
    assert!(
        matches!(
            gatekeeper_no_evidence,
            Err(pgmcp::db::queries::WorkItemOpError::Transition(
                TransitionError::EvidenceRequired { .. }
            ))
        ),
        "→verified requires passing evidence even for the gatekeeper; got {gatekeeper_no_evidence:?}"
    );

    // The item is still merely a verify candidate — never verified by the merge.
    let now: String = sqlx::query_scalar("SELECT status FROM work_items WHERE id = $1")
        .bind(item_id)
        .fetch_one(&pool)
        .await
        .expect("read status");
    assert_eq!(
        now, "verifying",
        "after a merge + refused verifies, the item rests at 'verifying', NOT 'verified'"
    );
}
