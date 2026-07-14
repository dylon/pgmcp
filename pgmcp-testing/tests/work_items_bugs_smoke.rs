//! Integration smoke test for the bug-tracking tools (v12 bug-tracker).
//!
//! Exercises the bug lifecycle end-to-end against real Postgres: create a
//! `kind='bug'` (born in `triage`, severity-derived priority, structured
//! sidecar) → an agent CANNOT confirm it → the user-token `work_item_triage`
//! confirms it (with the severity + reproduction gate) → `work_item_resolve`
//! closes a duplicate to `cancelled` and records a `duplicates` relation.
//! Self-skips (via `require_test_db!`) when `PGMCP_TEST_DATABASE_URL` is unset,
//! while still satisfying `query_inventory_vs_coverage` (which greps for a
//! `call_tool_cli("<tool>", …)` per dispatched tool — here `work_item_triage`
//! and `work_item_resolve`).

use std::sync::Arc;

use crate::common::text_of;
use arc_swap::ArcSwap;
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
/// tracker user-token (so the user-authority triage/resolve tools are testable).
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

/// Pull the `public_id` out of a `work_item_create` body (the row is serialized
/// at the top level).
fn public_id_of(v: &Value) -> String {
    v["public_id"]
        .as_str()
        .expect("row carries a public_id")
        .to_string()
}

#[tokio::test]
async fn work_item_bug_triage_resolve_flow() {
    let db = require_test_db!();
    let server = server_1024(db.pool().clone());

    // ── create a bug with severity + reproduction ──
    let bug = server
        .call_tool_cli(
            "work_item_create",
            json!({
                "kind": "bug",
                "title": "panic on empty input",
                "body": "the parser panics on an empty argument",
                "severity": "high",
                "reproduction_steps": "run `foo ''`",
                "expected_behavior": "a graceful error",
                "actual_behavior": "thread 'main' panicked",
            }),
        )
        .await
        .expect("work_item_create (bug) must succeed");
    let bv: Value = serde_json::from_str(&text_of(&bug)).expect("bug body JSON");
    assert_eq!(bv["kind"].as_str(), Some("bug"));
    assert_eq!(
        bv["status"].as_str(),
        Some("triage"),
        "a bug is born in triage (awaiting confirmation)"
    );
    assert_eq!(bv["severity"].as_str(), Some("high"));
    assert_eq!(
        bv["priority"].as_i64(),
        Some(70),
        "high severity seeds the default priority (70) when none is given"
    );
    let bug_id = public_id_of(&bv);

    // ── the structured sidecar is returned by work_item_get ──
    let got = server
        .call_tool_cli("work_item_get", json!({ "public_id": bug_id }))
        .await
        .expect("work_item_get must succeed");
    let gv: Value = serde_json::from_str(&text_of(&got)).expect("get body JSON");
    assert_eq!(
        gv["bug_details"]["reproduction_steps"].as_str(),
        Some("run `foo ''`"),
        "the bug-detail sidecar round-trips through get"
    );
    assert_eq!(
        gv["bug_details"]["actual_behavior"].as_str(),
        Some("thread 'main' panicked")
    );

    // ── HARD TRUST RULE: an agent may NOT confirm a bug (triage→confirmed is
    //    user-only; the generic set_status runs as Actor::Agent). ──
    assert!(
        server
            .call_tool_cli(
                "work_item_set_status",
                json!({ "public_id": bug_id, "status": "confirmed" }),
            )
            .await
            .is_err(),
        "an agent cannot move a bug to confirmed (user-token gate; no agent arm in the matrix)"
    );

    // ── work_item_triage with a WRONG token is refused ──
    assert!(
        server
            .call_tool_cli(
                "work_item_triage",
                json!({ "public_id": bug_id, "user_token": "WRONG" }),
            )
            .await
            .is_err(),
        "triage with a wrong user_token is refused (agents cannot confirm a bug)"
    );

    // ── work_item_triage with the correct token confirms it (severity + repro
    //    are already present from create) ──
    let triaged = server
        .call_tool_cli(
            "work_item_triage",
            json!({
                "public_id": bug_id,
                "user_token": "smoke-token",
                "root_cause": "missing bounds check on the argv slice",
            }),
        )
        .await
        .expect("triage with the correct token must succeed");
    let trv: Value = serde_json::from_str(&text_of(&triaged)).expect("triage body JSON");
    assert_eq!(trv["item"]["status"].as_str(), Some("confirmed"));
    assert!(
        !trv["bug_details"]["triaged_at"].is_null(),
        "triaged_at is stamped on confirmation"
    );
    assert_eq!(
        trv["bug_details"]["root_cause"].as_str(),
        Some("missing bounds check on the argv slice")
    );

    // ── a confirmed bug is workable: confirmed → in_progress is agent-legal ──
    let started = server
        .call_tool_cli(
            "work_item_set_status",
            json!({ "public_id": bug_id, "status": "in_progress", "reason": "fixing the panic" }),
        )
        .await
        .expect("confirmed → in_progress must succeed for an agent");
    assert_eq!(
        serde_json::from_str::<Value>(&text_of(&started)).unwrap()["status"].as_str(),
        Some("in_progress")
    );

    // ── confirm requires a severity: a bug created without one cannot be
    //    confirmed until a severity is supplied ──
    let bug2 = server
        .call_tool_cli(
            "work_item_create",
            json!({ "kind": "bug", "title": "no-severity bug", "reproduction_steps": "do X" }),
        )
        .await
        .expect("create bug2");
    let b2v: Value = serde_json::from_str(&text_of(&bug2)).expect("bug2 body JSON");
    assert!(b2v["severity"].is_null(), "bug2 has no severity yet");
    let bug2_id = public_id_of(&b2v);
    assert!(
        server
            .call_tool_cli(
                "work_item_triage",
                json!({ "public_id": bug2_id, "user_token": "smoke-token" }),
            )
            .await
            .is_err(),
        "a bug with no severity cannot be confirmed"
    );
    let t2 = server
        .call_tool_cli(
            "work_item_triage",
            json!({ "public_id": bug2_id, "user_token": "smoke-token", "severity": "low" }),
        )
        .await
        .expect("supplying a severity at triage time satisfies the gate");
    assert_eq!(
        serde_json::from_str::<Value>(&text_of(&t2)).unwrap()["item"]["status"].as_str(),
        Some("confirmed")
    );

    // ── confirm requires reproduction: a bug with severity but no repro cannot
    //    be confirmed ──
    let bug3 = server
        .call_tool_cli(
            "work_item_create",
            json!({ "kind": "bug", "title": "no-repro bug", "severity": "medium" }),
        )
        .await
        .expect("create bug3");
    let bug3_id = public_id_of(&serde_json::from_str::<Value>(&text_of(&bug3)).unwrap());
    assert!(
        server
            .call_tool_cli(
                "work_item_triage",
                json!({ "public_id": bug3_id, "user_token": "smoke-token" }),
            )
            .await
            .is_err(),
        "a bug with no reproduction_steps cannot be confirmed"
    );

    // ── work_item_resolve as a duplicate → cancelled + a 'duplicates' relation ──
    let resolved = server
        .call_tool_cli(
            "work_item_resolve",
            json!({
                "public_id": bug3_id,
                "user_token": "smoke-token",
                "resolution": "duplicate",
                "duplicate_of": bug_id,
                "reason": "same root cause as the first bug",
            }),
        )
        .await
        .expect("resolve as duplicate must succeed");
    let rv: Value = serde_json::from_str(&text_of(&resolved)).expect("resolve body JSON");
    assert_eq!(rv["item"]["status"].as_str(), Some("cancelled"));
    assert_eq!(rv["resolution"].as_str(), Some("duplicate"));
    assert_eq!(rv["bug_details"]["resolution"].as_str(), Some("duplicate"));

    let dup_relations: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM item_relations WHERE relation_type = 'duplicates'",
    )
    .fetch_one(db.pool())
    .await
    .expect("count duplicates relations");
    assert!(
        dup_relations >= 1,
        "resolution=duplicate recorded a 'duplicates' item_relation"
    );

    // ── resolve with a WRONG token is refused (agents cannot close a bug) ──
    assert!(
        server
            .call_tool_cli(
                "work_item_resolve",
                json!({ "public_id": bug2_id, "user_token": "WRONG", "resolution": "wont_fix", "reason": "x" }),
            )
            .await
            .is_err(),
        "resolve with a wrong user_token is refused"
    );

    // ── 'fixed' is NOT settable via resolve — it is reached via the evidence-
    //    backed verify path (work_item_attempt_verify) ──
    assert!(
        server
            .call_tool_cli(
                "work_item_resolve",
                json!({ "public_id": bug2_id, "user_token": "smoke-token", "resolution": "fixed", "reason": "x" }),
            )
            .await
            .is_err(),
        "resolution=fixed is reached via attempt_verify, not work_item_resolve"
    );

    // ── an unknown severity is rejected at create ──
    assert!(
        server
            .call_tool_cli(
                "work_item_create",
                json!({ "kind": "bug", "title": "bad sev", "severity": "apocalyptic" }),
            )
            .await
            .is_err(),
        "an unknown severity is rejected"
    );

    // ── work_item_resolve / triage apply only to bugs ──
    server
        .call_tool_cli(
            "work_item_create",
            json!({ "kind": "task", "title": "not a bug", "public_id": "bug-smoke-task" }),
        )
        .await
        .expect("create a non-bug task");
    assert!(
        server
            .call_tool_cli(
                "work_item_triage",
                json!({ "public_id": "bug-smoke-task", "user_token": "smoke-token", "severity": "low", "reproduction_steps": "x" }),
            )
            .await
            .is_err(),
        "work_item_triage rejects non-bug kinds"
    );
}
