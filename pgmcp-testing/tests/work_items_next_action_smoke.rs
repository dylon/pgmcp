//! Integration smoke test for the Phase-2 tracker ergonomics tools
//! (`~/.claude/plans/how-extensive-is-the-zazzy-galaxy.md`, "Tracker ergonomics
//! & next-action").
//!
//! Exercises all five new `work_item_*` tools end-to-end against real Postgres:
//!   - `work_item_assign` → `work_item_view my-work` lists the assigned item;
//!   - the `needs-triage` / `overdue` / `blocked` / `next-actionable` views;
//!   - `work_item_next_actionable` (the read-only frontier);
//!   - the AUTO-UNBLOCK end-to-end invariant: B depends_on A, B is blocked,
//!     A is driven to `verified` through the criterion + trusted-`ci`-evidence +
//!     `work_item_attempt_verify` gatekeeper path; B is then auto-`ready` and
//!     `work_item_history(B)` carries an `actor_kind='system'` blocked→ready
//!     status event;
//!   - `work_item_bulk` partial-success (one illegal transition → `failed`, one
//!     legal → counted in `succeeded`).
//!
//! Self-skips (via `require_test_db!`) when `PGMCP_TEST_DATABASE_URL` is unset,
//! while still satisfying `query_inventory_vs_coverage` (which greps these
//! source files for a `call_tool_cli("<tool>", …)` per dispatched tool — here
//! `work_item_view`, `work_item_next_actionable`, `work_item_assign`,
//! `work_item_history`, and `work_item_bulk`).

mod common;

use std::sync::Arc;

use arc_swap::ArcSwap;
use common::text_of;
use pgmcp::config::Config;
use pgmcp::context::SystemContext;
use pgmcp::db::DbClient;
use pgmcp::embed::EmbedSource;
use pgmcp::mcp::logging::LogBroadcaster;
use pgmcp::mcp::server::McpServer;
use pgmcp::mcp::tasks::TaskStore;
use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::mocks::DeterministicEmbeddingBackend;
use pgmcp_testing::require_test_db;
use serde_json::{Value, json};
use sqlx::PgPool;

/// Server with a real pool, a 1024-d deterministic embedder, and a configured
/// tracker user-token (parity with the sibling tracker harnesses).
fn server_1024(pool: PgPool) -> McpServer {
    let db: Arc<dyn DbClient> = Arc::new(pool);
    let stats = Arc::new(StatsTracker::new());
    let mut cfg = Config::default();
    cfg.tracker.user_token = Some("smoke-token".to_string());
    let config = Arc::new(ArcSwap::from_pointee(cfg));
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

/// Does the `items` array of a `work_item_view` body contain `public_id`?
fn view_contains(body: &Value, public_id: &str) -> bool {
    body["items"]
        .as_array()
        .map(|a| a.iter().any(|r| r["public_id"].as_str() == Some(public_id)))
        .unwrap_or(false)
}

#[tokio::test]
async fn work_item_views_assign_history() {
    let db = require_test_db!();
    let server = server_1024(db.pool().clone());

    // ── a task we will assign + an overdue task + a triage bug ──
    let mine = server
        .call_tool_cli(
            "work_item_create",
            json!({ "kind": "task", "title": "view-mine", "public_id": "view-mine" }),
        )
        .await
        .expect("create the assignable task");
    let mine_id = public_id_of(&serde_json::from_str::<Value>(&text_of(&mine)).unwrap());
    assert_eq!(mine_id, "view-mine");

    // ── work_item_assign: durable ownership to the "cli" caller identity ──
    let assigned = server
        .call_tool_cli(
            "work_item_assign",
            json!({ "public_id": "view-mine", "assignee": "cli", "assigned_by": "tester" }),
        )
        .await
        .expect("work_item_assign must succeed");
    let av: Value = serde_json::from_str(&text_of(&assigned)).expect("assign body JSON");
    assert_eq!(av["assigned"].as_bool(), Some(true));
    assert_eq!(av["item"]["assignee"].as_str(), Some("cli"));
    assert!(
        !av["item"]["assigned_at"].is_null(),
        "assigned_at is stamped on assignment"
    );

    // ── work_item_view my-work: with no assignee param the CLI path defaults to
    //    the "cli" sentinel, so our just-assigned item shows up. ──
    let my_work = server
        .call_tool_cli("work_item_view", json!({ "view": "my-work" }))
        .await
        .expect("work_item_view my-work must succeed");
    let mwv: Value = serde_json::from_str(&text_of(&my_work)).expect("my-work body JSON");
    assert_eq!(mwv["view"].as_str(), Some("my-work"));
    assert!(
        view_contains(&mwv, "view-mine"),
        "my-work lists the item assigned to the caller (cli)"
    );

    // An explicit assignee scopes the same view to another owner (empty here).
    let other = server
        .call_tool_cli(
            "work_item_view",
            json!({ "view": "my-work", "assignee": "nobody-zzz" }),
        )
        .await
        .expect("work_item_view my-work (other assignee) must succeed");
    let ov: Value = serde_json::from_str(&text_of(&other)).expect("other body JSON");
    assert!(
        !view_contains(&ov, "view-mine"),
        "scoping my-work to a different assignee excludes our item"
    );

    // ── unassign: empty assignee clears ownership; my-work no longer lists it ──
    let unassigned = server
        .call_tool_cli(
            "work_item_assign",
            json!({ "public_id": "view-mine", "assignee": "" }),
        )
        .await
        .expect("unassign must succeed");
    let uav: Value = serde_json::from_str(&text_of(&unassigned)).expect("unassign body JSON");
    assert_eq!(uav["assigned"].as_bool(), Some(false));
    assert!(uav["item"]["assignee"].is_null(), "assignee cleared");
    assert!(
        uav["item"]["assigned_at"].is_null(),
        "assigned_at cleared on unassign"
    );
    // Re-assign so later assertions (and the history tool) still have an owner.
    server
        .call_tool_cli(
            "work_item_assign",
            json!({ "public_id": "view-mine", "assignee": "cli" }),
        )
        .await
        .expect("re-assign must succeed");

    // ── needs-triage view: a freshly-created bug is born in triage ──
    server
        .call_tool_cli(
            "work_item_create",
            json!({
                "kind": "bug",
                "title": "view-bug",
                "public_id": "view-bug",
                "severity": "low",
                "reproduction_steps": "do X",
            }),
        )
        .await
        .expect("create a triage bug");
    let triage = server
        .call_tool_cli("work_item_view", json!({ "view": "needs-triage" }))
        .await
        .expect("needs-triage view must succeed");
    let trv: Value = serde_json::from_str(&text_of(&triage)).expect("triage view JSON");
    assert!(
        view_contains(&trv, "view-bug"),
        "needs-triage lists the kind='bug' status='triage' item"
    );

    // ── overdue view: set a past due_at on the assignable task ──
    server
        .call_tool_cli(
            "work_item_update",
            json!({ "public_id": "view-mine", "due_at": "2000-01-01T00:00:00Z" }),
        )
        .await
        .expect("set a past due date");
    let overdue = server
        .call_tool_cli("work_item_view", json!({ "view": "overdue" }))
        .await
        .expect("overdue view must succeed");
    let odv: Value = serde_json::from_str(&text_of(&overdue)).expect("overdue view JSON");
    assert!(
        view_contains(&odv, "view-mine"),
        "overdue lists the past-due, not-yet-closed item"
    );

    // ── work_item_history: the assignable task has at least its create-era
    //    status event(s); the tool returns a chronological timeline. ──
    let hist = server
        .call_tool_cli("work_item_history", json!({ "public_id": "view-mine" }))
        .await
        .expect("work_item_history must succeed");
    let hv: Value = serde_json::from_str(&text_of(&hist)).expect("history JSON");
    assert_eq!(hv["public_id"].as_str(), Some("view-mine"));
    assert!(
        hv["timeline"].is_array(),
        "history returns a timeline array"
    );
}

#[tokio::test]
async fn work_item_next_actionable_and_blocked_views() {
    let db = require_test_db!();
    let server = server_1024(db.pool().clone());

    // A ready item with no blocker is workable-now; a blocked item is not.
    server
        .call_tool_cli(
            "work_item_create",
            json!({ "kind": "task", "title": "na-ready", "public_id": "na-ready", "priority": 9 }),
        )
        .await
        .expect("create na-ready");
    server
        .call_tool_cli(
            "work_item_set_status",
            json!({ "public_id": "na-ready", "status": "ready" }),
        )
        .await
        .expect("mark na-ready ready");

    server
        .call_tool_cli(
            "work_item_create",
            json!({ "kind": "task", "title": "na-blocked", "public_id": "na-blocked" }),
        )
        .await
        .expect("create na-blocked");
    server
        .call_tool_cli(
            "work_item_set_status",
            json!({ "public_id": "na-blocked", "status": "blocked", "reason": "waiting" }),
        )
        .await
        .expect("mark na-blocked blocked");

    // ── work_item_next_actionable: the ready item is in the frontier; the
    //    blocked one is not. ──
    let na = server
        .call_tool_cli("work_item_next_actionable", json!({ "limit": 100 }))
        .await
        .expect("work_item_next_actionable must succeed");
    let nav: Value = serde_json::from_str(&text_of(&na)).expect("next-actionable JSON");
    let actionable = nav["actionable"].as_array().expect("actionable array");
    assert!(
        actionable
            .iter()
            .any(|r| r["public_id"].as_str() == Some("na-ready")),
        "the ready, unblocked item is actionable now"
    );
    assert!(
        !actionable
            .iter()
            .any(|r| r["public_id"].as_str() == Some("na-blocked")),
        "the blocked item is NOT actionable"
    );

    // ── blocked view: the blocked item shows; next-actionable view agrees with
    //    the dedicated tool. ──
    let blocked = server
        .call_tool_cli("work_item_view", json!({ "view": "blocked" }))
        .await
        .expect("blocked view must succeed");
    let bv: Value = serde_json::from_str(&text_of(&blocked)).expect("blocked view JSON");
    assert!(
        bv["items"]
            .as_array()
            .map(|a| a
                .iter()
                .any(|r| r["public_id"].as_str() == Some("na-blocked")))
            .unwrap_or(false),
        "the blocked view lists the blocked item"
    );

    let na_view = server
        .call_tool_cli("work_item_view", json!({ "view": "next-actionable" }))
        .await
        .expect("next-actionable view must succeed");
    let nvv: Value = serde_json::from_str(&text_of(&na_view)).expect("na view JSON");
    assert!(
        nvv["items"]
            .as_array()
            .map(|a| a
                .iter()
                .any(|r| r["public_id"].as_str() == Some("na-ready")))
            .unwrap_or(false),
        "the next-actionable view agrees with the dedicated tool"
    );
}

#[tokio::test]
async fn work_item_auto_unblock_end_to_end() {
    let db = require_test_db!();
    let server = server_1024(db.pool().clone());

    // ── A (the blocker) and B (the dependent) ──
    server
        .call_tool_cli(
            "work_item_create",
            json!({ "kind": "task", "title": "blocker A", "public_id": "aub-a" }),
        )
        .await
        .expect("create A");
    server
        .call_tool_cli(
            "work_item_create",
            json!({ "kind": "task", "title": "dependent B", "public_id": "aub-b" }),
        )
        .await
        .expect("create B");

    // B depends_on A.
    server
        .call_tool_cli(
            "work_item_link",
            json!({ "from_public_id": "aub-b", "to_public_id": "aub-a", "relation_type": "depends_on" }),
        )
        .await
        .expect("link B depends_on A");

    // B is blocked.
    server
        .call_tool_cli(
            "work_item_set_status",
            json!({ "public_id": "aub-b", "status": "blocked", "reason": "waiting on A" }),
        )
        .await
        .expect("block B");

    // ── Drive A → verified through the gatekeeper path. An agent cannot
    //    self-verify; only passing TRUSTED-source evidence on every required
    //    criterion does. Add a criterion, then insert trusted `ci` evidence
    //    directly (the MCP record_evidence tool forces source='manual', which
    //    cannot satisfy the gate — exactly the trust boundary under test). ──
    let crit = server
        .call_tool_cli(
            "work_item_add_criterion",
            json!({
                "public_id": "aub-a",
                "criterion_kind": "test",
                "description": "A is proven by a passing CI run",
            }),
        )
        .await
        .expect("add a required criterion to A");
    let crit_id = serde_json::from_str::<Value>(&text_of(&crit)).unwrap()["criterion_id"]
        .as_i64()
        .expect("criterion_id present");

    // Trusted CI evidence (source='ci', verdict='pass') for that criterion.
    sqlx::query(
        "INSERT INTO verification_evidence
            (criterion_id, item_id, verdict, source)
         SELECT $1, ac.item_id, 'pass', 'ci'
           FROM acceptance_criteria ac WHERE ac.id = $1",
    )
    .bind(crit_id)
    .execute(db.pool())
    .await
    .expect("insert trusted ci evidence");

    // A self-reports done, then the gatekeeper verifies it (trusted evidence).
    // claimed_done is only reachable from in_progress (or rejected) in the
    // matrix, so start A first.
    server
        .call_tool_cli(
            "work_item_set_status",
            json!({ "public_id": "aub-a", "status": "in_progress", "reason": "starting A" }),
        )
        .await
        .expect("A → in_progress");
    server
        .call_tool_cli(
            "work_item_set_status",
            json!({ "public_id": "aub-a", "status": "claimed_done" }),
        )
        .await
        .expect("A → claimed_done (agent self-report)");
    let verified = server
        .call_tool_cli("work_item_attempt_verify", json!({ "public_id": "aub-a" }))
        .await
        .expect("attempt_verify must succeed (trusted ci evidence present)");
    assert_eq!(
        serde_json::from_str::<Value>(&text_of(&verified)).unwrap()["status"].as_str(),
        Some("verified"),
        "A is verified through the gatekeeper path"
    );

    // ── INVARIANT: verifying A auto-unblocked B (its only blocker cleared). B
    //    is now `ready`. ──
    let b_after = server
        .call_tool_cli("work_item_get", json!({ "public_id": "aub-b" }))
        .await
        .expect("get B after A verified");
    assert_eq!(
        serde_json::from_str::<Value>(&text_of(&b_after)).unwrap()["item"]["status"].as_str(),
        Some("ready"),
        "B was auto-unblocked (blocked → ready) when its last blocker verified"
    );

    // ── …and the auto-unblock is recorded as an actor_kind='system' status event
    //    in B's timeline (visible through work_item_history). ──
    let b_hist = server
        .call_tool_cli("work_item_history", json!({ "public_id": "aub-b" }))
        .await
        .expect("work_item_history(B) must succeed");
    let bhv: Value = serde_json::from_str(&text_of(&b_hist)).expect("B history JSON");
    let timeline = bhv["timeline"].as_array().expect("B timeline array");
    assert!(
        timeline.iter().any(|e| {
            e["kind"].as_str() == Some("status")
                && e["detail"]["actor_kind"].as_str() == Some("system")
                && e["detail"]["to"].as_str() == Some("ready")
                && e["detail"]["from"].as_str() == Some("blocked")
        }),
        "B's history carries the system-actor blocked→ready auto-unblock event; got {timeline:#?}"
    );
}

#[tokio::test]
async fn work_item_bulk_partial_success() {
    let db = require_test_db!();
    let server = server_1024(db.pool().clone());

    // One item we can legally start (pending → in_progress) and one that is
    // terminal (cancelled), so the SAME bulk set_status partially fails.
    server
        .call_tool_cli(
            "work_item_create",
            json!({ "kind": "task", "title": "bulk-legal", "public_id": "bulk-legal" }),
        )
        .await
        .expect("create bulk-legal");
    server
        .call_tool_cli(
            "work_item_create",
            json!({ "kind": "task", "title": "bulk-illegal", "public_id": "bulk-illegal" }),
        )
        .await
        .expect("create bulk-illegal");
    // Move bulk-illegal to `triage` (agent-legal from pending). The matrix has
    // NO `triage → in_progress` arm, so the SAME bulk set_status → in_progress
    // is illegal for it (lands in `failed`) while it is legal for the pending
    // bulk-legal item — a clean partial-success split that needs no user token.
    server
        .call_tool_cli(
            "work_item_set_status",
            json!({ "public_id": "bulk-illegal", "status": "triage", "reason": "park in triage for the test" }),
        )
        .await
        .expect("move bulk-illegal to triage (agent-legal)");

    // ── bulk set_status → in_progress over BOTH: the pending one succeeds, the
    //    triage one is an illegal transition and lands in `failed`. ──
    let bulk = server
        .call_tool_cli(
            "work_item_bulk",
            json!({
                "op": "set_status",
                "public_ids": ["bulk-legal", "bulk-illegal"],
                "status": "in_progress",
                "reason": "bulk start",
            }),
        )
        .await
        .expect("work_item_bulk must succeed (partial-success envelope)");
    let bv: Value = serde_json::from_str(&text_of(&bulk)).expect("bulk JSON");
    assert_eq!(bv["op"].as_str(), Some("set_status"));
    assert_eq!(bv["attempted"].as_i64(), Some(2));
    assert_eq!(
        bv["succeeded"].as_i64(),
        Some(1),
        "exactly the legal transition applied"
    );
    let failed = bv["failed"].as_array().expect("failed array");
    assert_eq!(failed.len(), 1, "exactly the illegal transition failed");
    assert_eq!(
        failed[0]["public_id"].as_str(),
        Some("bulk-illegal"),
        "the terminal item is the one that failed"
    );
    assert!(
        failed[0]["error"].as_str().is_some(),
        "the failure carries an explanatory error string"
    );

    // The legal item really did transition.
    let legal = server
        .call_tool_cli("work_item_get", json!({ "public_id": "bulk-legal" }))
        .await
        .expect("get bulk-legal");
    assert_eq!(
        serde_json::from_str::<Value>(&text_of(&legal)).unwrap()["item"]["status"].as_str(),
        Some("in_progress"),
        "the legal target was actually transitioned by the bulk op"
    );

    // ── bulk assign by explicit ids (a non-transition op) ──
    let bassign = server
        .call_tool_cli(
            "work_item_bulk",
            json!({ "op": "assign", "public_ids": ["bulk-legal"], "assignee": "bulk-owner" }),
        )
        .await
        .expect("bulk assign must succeed");
    assert_eq!(
        serde_json::from_str::<Value>(&text_of(&bassign)).unwrap()["succeeded"].as_i64(),
        Some(1)
    );

    // An unknown op is rejected.
    assert!(
        server
            .call_tool_cli(
                "work_item_bulk",
                json!({ "op": "frobnicate", "public_ids": ["bulk-legal"] }),
            )
            .await
            .is_err(),
        "an unknown bulk op is rejected"
    );

    // Selecting no targets (neither public_ids nor view) is rejected.
    assert!(
        server
            .call_tool_cli("work_item_bulk", json!({ "op": "assign" }))
            .await
            .is_err(),
        "a bulk op with no target selector is rejected"
    );
}
