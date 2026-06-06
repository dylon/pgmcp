//! Integration smoke tests for the work-item / plan tracker MCP tools.
//!
//! Exercises all seven `work_item_*` tools end-to-end against real Postgres:
//! create (root + child) → get (with subtree) → update → list → tree →
//! reparent → set_status. Self-skips (via `require_test_db!`) when
//! `PGMCP_TEST_DATABASE_URL` is unset, so it stays green for contributors
//! without a local Postgres+pgvector — while still satisfying
//! `query_inventory_vs_coverage` (which greps these source files for a
//! `call_tool_cli("<tool>", …)` per dispatched tool).
//!
//! Uses a local 1024-d deterministic embedder (`server_1024`) because the
//! `work_items.embedding` column is `vector(1024)` (BGE-M3), matching the
//! experiment-tool integration harness. The tracker `create` tool leaves the
//! embedding NULL (cron backfills), so the dimension is not load-bearing here,
//! but the 1024-d embedder keeps the harness identical to its sibling.

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
use pgmcp::tracker::status::WorkItemStatus;
use pgmcp::tracker::transition::Actor;
use pgmcp_testing::mocks::DeterministicEmbeddingBackend;
use pgmcp_testing::require_test_db;
use serde_json::{Value, json};
use sqlx::PgPool;

fn server_with_embed_dim(pool: PgPool, dim: usize) -> McpServer {
    let db: Arc<dyn DbClient> = Arc::new(pool);
    let stats = Arc::new(StatsTracker::new());
    let mut cfg = Config::default();
    // A user-token so the defer/reinstate (user-authority) tools are testable.
    cfg.tracker.user_token = Some("smoke-token".to_string());
    let config = Arc::new(ArcSwap::from_pointee(cfg));
    let log_broadcaster = Arc::new(LogBroadcaster::new());
    let task_store = Arc::new(TaskStore::new());
    let embed_backend: Arc<dyn pgmcp::embed::EmbeddingBackend> =
        Arc::new(DeterministicEmbeddingBackend::new(dim));
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

/// Server with a real pool and a 1024-d deterministic embedder (matches the
/// `work_items.embedding vector(1024)` column).
fn server_1024(pool: PgPool) -> McpServer {
    server_with_embed_dim(pool, 1024)
}

/// Pull the `public_id` out of a `work_item_create` / `_update` body (the row
/// is serialized at the top level).
fn public_id_of(v: &Value) -> String {
    v["public_id"]
        .as_str()
        .expect("row carries a public_id")
        .to_string()
}

#[tokio::test]
async fn work_item_tracker_full_round_trip() {
    let db = require_test_db!();
    let server = server_1024(db.pool().clone());

    // ── create a root plan ──
    let plan = server
        .call_tool_cli(
            "work_item_create",
            json!({
                "kind": "plan",
                "title": "Ship the work-item tracker",
                "body": "Phase 1: CRUD + lifecycle MCP tools.",
                "priority": 10,
            }),
        )
        .await
        .expect("work_item_create (plan) must succeed");
    let pv: Value = serde_json::from_str(&text_of(&plan)).expect("plan body JSON");
    assert_eq!(pv["kind"].as_str(), Some("plan"));
    assert_eq!(pv["status"].as_str(), Some("pending"), "default status");
    assert_eq!(pv["origin"].as_str(), Some("agent_write"), "agent origin");
    assert!(pv["parent_id"].is_null(), "a root has no parent");
    let plan_id = public_id_of(&pv);

    // Input normalization and caller-facing validation happen before writes.
    let explicit_id = format!("normalized-{plan_id}");
    let normalized = server
        .call_tool_cli(
            "work_item_create",
            json!({
                "kind": " task ",
                "title": "  Normalize create input  ",
                "body": "   ",
                "public_id": format!(" {explicit_id} "),
                "priority": 100,
                "weight": 0.25,
                "parametric_corpus": " corpus/** ",
            }),
        )
        .await
        .expect("normalized create must succeed");
    let nv: Value = serde_json::from_str(&text_of(&normalized)).expect("normalized body JSON");
    assert_eq!(nv["public_id"].as_str(), Some(explicit_id.as_str()));
    assert_eq!(nv["kind"].as_str(), Some("task"));
    assert_eq!(nv["title"].as_str(), Some("Normalize create input"));
    assert!(nv["body"].is_null(), "blank body normalizes to NULL");
    assert_eq!(nv["priority"].as_i64(), Some(100));
    assert_eq!(nv["parametric_corpus"].as_str(), Some("corpus/**"));

    assert!(
        server
            .call_tool_cli(
                "work_item_create",
                json!({ "kind": "task", "title": "bad priority", "priority": 101 }),
            )
            .await
            .is_err(),
        "priority above 100 is rejected before the DB CHECK"
    );
    assert!(
        server
            .call_tool_cli(
                "work_item_create",
                json!({ "kind": "task", "title": "bad weight", "weight": 0.0 }),
            )
            .await
            .is_err(),
        "non-positive weight is rejected before the DB CHECK"
    );
    assert!(
        server
            .call_tool_cli(
                "work_item_create",
                json!({ "kind": "task", "title": "bad project", "project": "missing-project" }),
            )
            .await
            .is_err(),
        "an unknown project name must not silently create a global item"
    );
    assert!(
        server
            .call_tool_cli(
                "work_item_create",
                json!({ "kind": "task", "title": "not a bug", "severity": "low" }),
            )
            .await
            .is_err(),
        "severity is reserved for first-class bugs"
    );
    assert!(
        server
            .call_tool_cli(
                "work_item_create",
                json!({ "kind": "task", "title": "not a bug", "reproduction_steps": "do X" }),
            )
            .await
            .is_err(),
        "bug-detail sidecar fields are reserved for first-class bugs"
    );

    // ── create a child task under the plan ──
    let task = server
        .call_tool_cli(
            "work_item_create",
            json!({
                "kind": "task",
                "title": "Wire the dispatch arms",
                "parent_public_id": plan_id,
                "priority": 5,
            }),
        )
        .await
        .expect("work_item_create (child task) must succeed");
    let tv: Value = serde_json::from_str(&text_of(&task)).expect("task body JSON");
    assert_eq!(tv["kind"].as_str(), Some("task"));
    assert!(!tv["parent_id"].is_null(), "child carries a parent_id");
    let task_id = public_id_of(&tv);

    // An unknown kind is rejected.
    assert!(
        server
            .call_tool_cli(
                "work_item_create",
                json!({ "kind": "not_a_kind", "title": "x" }),
            )
            .await
            .is_err(),
        "an unknown kind must be rejected"
    );

    // An empty title is rejected.
    assert!(
        server
            .call_tool_cli(
                "work_item_create",
                json!({ "kind": "task", "title": "   " }),
            )
            .await
            .is_err(),
        "an empty title must be rejected"
    );

    // ── get (plain) ──
    let got = server
        .call_tool_cli(
            "work_item_get",
            json!({ "public_id": format!(" {plan_id} ") }),
        )
        .await
        .expect("work_item_get trims public_id and succeeds");
    let gv: Value = serde_json::from_str(&text_of(&got)).expect("get body JSON");
    assert_eq!(gv["item"]["public_id"].as_str(), Some(plan_id.as_str()));
    assert!(
        gv.get("subtree").is_none(),
        "subtree omitted unless requested"
    );

    // ── get (with subtree) ──
    let got_tree = server
        .call_tool_cli(
            "work_item_get",
            json!({ "public_id": plan_id, "include_subtree": true }),
        )
        .await
        .expect("work_item_get (subtree) must succeed");
    let gtv: Value = serde_json::from_str(&text_of(&got_tree)).expect("get+subtree body JSON");
    let subtree = gtv["subtree"].as_array().expect("subtree is an array");
    assert!(
        subtree.len() >= 2,
        "subtree includes the plan and its child task; got {}",
        subtree.len()
    );

    // A missing public_id is rejected.
    assert!(
        server
            .call_tool_cli(
                "work_item_get",
                json!({ "public_id": "does-not-exist-000000" })
            )
            .await
            .is_err(),
        "get of a missing item must be rejected"
    );

    // ── update ──
    let updated = server
        .call_tool_cli(
            "work_item_update",
            json!({
                "public_id": format!(" {task_id} "),
                "title": "  Wire the dispatch arms + tests  ",
                "priority": 7,
            }),
        )
        .await
        .expect("work_item_update must succeed");
    let uv: Value = serde_json::from_str(&text_of(&updated)).expect("update body JSON");
    assert_eq!(uv["title"].as_str(), Some("Wire the dispatch arms + tests"));
    assert_eq!(uv["priority"].as_i64(), Some(7));
    assert!(
        server
            .call_tool_cli(
                "work_item_update",
                json!({ "public_id": task_id, "title": "   " }),
            )
            .await
            .is_err(),
        "blank update title is rejected"
    );
    assert!(
        server
            .call_tool_cli(
                "work_item_update",
                json!({ "public_id": task_id, "priority": 101 }),
            )
            .await
            .is_err(),
        "update priority above 100 is rejected before the DB CHECK"
    );
    assert!(
        server
            .call_tool_cli(
                "work_item_update",
                json!({ "public_id": task_id, "weight": 0.0 }),
            )
            .await
            .is_err(),
        "update non-positive weight is rejected before the DB CHECK"
    );
    assert!(
        server
            .call_tool_cli(
                "work_item_update",
                json!({ "public_id": task_id, "severity": "low" }),
            )
            .await
            .is_err(),
        "bug-only update fields are rejected on non-bugs"
    );

    // ── list (filter by kind) ──
    let listed = server
        .call_tool_cli("work_item_list", json!({ "kind": " task " }))
        .await
        .expect("work_item_list trims kind filters");
    let lv: Value = serde_json::from_str(&text_of(&listed)).expect("list body JSON");
    let rows = lv.as_array().expect("list returns an array");
    assert!(
        rows.iter()
            .any(|r| r["public_id"].as_str() == Some(task_id.as_str())),
        "the listed tasks include our task"
    );
    assert!(
        server
            .call_tool_cli("work_item_list", json!({ "kind": "not_a_kind" }))
            .await
            .is_err(),
        "unknown list kind is rejected"
    );
    assert!(
        server
            .call_tool_cli("work_item_list", json!({ "status": "not_a_status" }))
            .await
            .is_err(),
        "unknown list status is rejected"
    );
    assert!(
        server
            .call_tool_cli("work_item_list", json!({ "project": "missing-project" }))
            .await
            .is_err(),
        "unknown list project does not fall back to global rows"
    );

    // ── list (children of the plan) ──
    let children = server
        .call_tool_cli(
            "work_item_list",
            json!({ "parent_public_id": format!(" {plan_id} "), "limit": 100 }),
        )
        .await
        .expect("work_item_list trims parent_public_id");
    let cv: Value = serde_json::from_str(&text_of(&children)).expect("children body JSON");
    assert_eq!(
        cv.as_array().map(|a| a.len()),
        Some(1),
        "the plan has exactly one direct child"
    );

    // ── tree ──
    let tree = server
        .call_tool_cli("work_item_tree", json!({ "public_id": plan_id }))
        .await
        .expect("work_item_tree must succeed");
    let trv: Value = serde_json::from_str(&text_of(&tree)).expect("tree body JSON");
    assert!(
        trv.as_array().map(|a| a.len() >= 2).unwrap_or(false),
        "tree returns the plan + descendants"
    );

    // ── reparent: detach the task to a root, then re-attach under the plan ──
    let detached = server
        .call_tool_cli("work_item_reparent", json!({ "public_id": task_id }))
        .await
        .expect("work_item_reparent (to root) must succeed");
    let dv: Value = serde_json::from_str(&text_of(&detached)).expect("reparent body JSON");
    assert!(dv["parent_id"].is_null(), "task is now a root");

    server
        .call_tool_cli(
            "work_item_reparent",
            json!({ "public_id": task_id, "new_parent_public_id": plan_id }),
        )
        .await
        .expect("work_item_reparent (back under plan) must succeed");

    // Cycle guard: cannot reparent the plan under its own descendant (the task).
    assert!(
        server
            .call_tool_cli(
                "work_item_reparent",
                json!({ "public_id": plan_id, "new_parent_public_id": task_id }),
            )
            .await
            .is_err(),
        "reparenting an item under its own descendant must be rejected (cycle)"
    );
    // Cycle guard: cannot reparent an item under itself.
    assert!(
        server
            .call_tool_cli(
                "work_item_reparent",
                json!({ "public_id": plan_id, "new_parent_public_id": plan_id }),
            )
            .await
            .is_err(),
        "reparenting an item under itself must be rejected"
    );

    // ── set_status: a legal agent transition (pending → in_progress) ──
    let started = server
        .call_tool_cli(
            "work_item_set_status",
            json!({
                "public_id": format!(" {task_id} "),
                "status": " in_progress ",
                "reason": " starting the wiring ",
            }),
        )
        .await
        .expect("work_item_set_status (pending→in_progress) must succeed");
    let sv: Value = serde_json::from_str(&text_of(&started)).expect("set_status body JSON");
    assert_eq!(sv["status"].as_str(), Some("in_progress"));
    assert!(
        !sv["started_at"].is_null(),
        "started_at stamped on first start"
    );
    let stored_reason: String = sqlx::query_scalar(
        "SELECT reason FROM work_item_status_history
         WHERE item_id = $1 AND to_status = 'in_progress'
         ORDER BY id DESC LIMIT 1",
    )
    .bind(sv["id"].as_i64().expect("work item id"))
    .fetch_one(db.pool())
    .await
    .expect("status history reason");
    assert_eq!(stored_reason, "starting the wiring");

    // ── HARD TRUST RULE: an agent may NOT self-verify. ──
    let verify_attempt = server
        .call_tool_cli(
            "work_item_set_status",
            json!({ "public_id": task_id, "status": "verified" }),
        )
        .await;
    assert!(
        verify_attempt.is_err(),
        "an agent requesting →verified must be refused (gatekeeper-only transition)"
    );

    // An unknown status is rejected.
    assert!(
        server
            .call_tool_cli(
                "work_item_set_status",
                json!({ "public_id": task_id, "status": "done" }),
            )
            .await
            .is_err(),
        "an unknown status must be rejected"
    );

    // ── completion roll-up: the only leaf (the in_progress task) is unverified,
    //    so verified_pct is 0 while the subtree has countable leaves. ──
    let completion = server
        .call_tool_cli("work_item_completion", json!({ "public_id": plan_id }))
        .await
        .expect("work_item_completion must succeed");
    let comp: Value = serde_json::from_str(&text_of(&completion)).expect("completion body JSON");
    assert_eq!(
        comp["verified_pct"].as_f64(),
        Some(0.0),
        "no leaf is evidence-verified yet"
    );
    assert!(
        comp["leaf_count"].as_i64().unwrap_or(0) >= 1,
        "the subtree has at least one countable leaf"
    );

    // ── reprioritize: rescores active items and returns a now/next/later plan ──
    let replan = server
        .call_tool_cli("work_item_reprioritize", json!({ "limit": 50 }))
        .await
        .expect("work_item_reprioritize must succeed");
    let rp: Value = serde_json::from_str(&text_of(&replan)).expect("reprioritize body JSON");
    assert!(rp["now"].is_array(), "reprioritize returns a now bucket");
    assert!(
        rp["shown"].as_i64().unwrap_or(0) >= 1,
        "at least one active item was rescored and shown"
    );

    // ── semantic search: items were embedded on create, so the query returns
    //    a hits array (the deterministic test embedder makes ordering stable). ──
    let found = server
        .call_tool_cli(
            "work_item_search",
            json!({ "query": "work-item tracker", "limit": 10 }),
        )
        .await
        .expect("work_item_search must succeed");
    let fv: Value = serde_json::from_str(&text_of(&found)).expect("search body JSON");
    assert!(fv["hits"].is_array(), "search returns a hits array");
    assert_eq!(
        fv["limit"].as_i64(),
        Some(10),
        "search echoes the normalized limit"
    );
    assert!(
        fv["hits"]
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false),
        "the created items were embedded on write, so search finds them"
    );

    // ── plan definition + validation: a rule "a plan must have a task child"
    //    is satisfied by our plan→task tree. ──
    server
        .call_tool_cli(
            "plan_define",
            json!({
                "title": "Basic plan shape",
                "slug": "basic-plan-shape",
                "rules": [
                    { "rule_kind": "required_child_kind", "applies_to_kind": "plan", "child_kind": "task" }
                ]
            }),
        )
        .await
        .expect("plan_define must succeed");
    let validated = server
        .call_tool_cli(
            "plan_validate",
            json!({ "root_public_id": plan_id, "definition_slug": "basic-plan-shape" }),
        )
        .await
        .expect("plan_validate must succeed");
    let vr: Value = serde_json::from_str(&text_of(&validated)).expect("validate body JSON");
    assert_eq!(
        vr["valid"].as_bool(),
        Some(true),
        "the plan has a task child, satisfying required_child_kind"
    );

    // ── verification gatekeeping (the trust crown jewel) ──
    // An agent can add a criterion and record MANUAL evidence, but
    // attempt_verify is REFUSED — manual is not a trusted source.
    let crit = server
        .call_tool_cli(
            "work_item_add_criterion",
            json!({
                "public_id": task_id,
                "criterion_kind": "test",
                "description": "the dispatch wiring is exercised by a test",
            }),
        )
        .await
        .expect("work_item_add_criterion must succeed");
    let cv: Value = serde_json::from_str(&text_of(&crit)).expect("criterion body JSON");
    let crit_id = cv["criterion_id"].as_i64().expect("criterion_id present");

    let ev = server
        .call_tool_cli(
            "work_item_record_evidence",
            json!({ "criterion_id": crit_id, "verdict": "pass" }),
        )
        .await
        .expect("work_item_record_evidence (manual) must succeed");
    let ev_v: Value = serde_json::from_str(&text_of(&ev)).expect("evidence body JSON");
    assert_eq!(
        ev_v["source"].as_str(),
        Some("manual"),
        "MCP evidence is forced to manual"
    );

    // Agent self-reports completion (claimed_done), then attempts verification.
    server
        .call_tool_cli(
            "work_item_set_status",
            json!({ "public_id": task_id, "status": "claimed_done" }),
        )
        .await
        .expect("set_status claimed_done must succeed (agent self-report)");

    assert!(
        server
            .call_tool_cli("work_item_attempt_verify", json!({ "public_id": task_id }))
            .await
            .is_err(),
        "attempt_verify must be REFUSED: only manual (untrusted) evidence exists — an agent cannot self-verify"
    );

    // ── defer is USER-only: a wrong token is refused; the right token works;
    //    then reinstate brings it back to in_progress. ──
    assert!(
        server
            .call_tool_cli(
                "work_item_defer",
                json!({ "public_id": task_id, "reason": "x", "user_token": "WRONG" }),
            )
            .await
            .is_err(),
        "defer with a wrong user_token must be refused (agents cannot self-defer)"
    );
    let deferred = server
        .call_tool_cli(
            "work_item_defer",
            json!({ "public_id": task_id, "reason": "out of scope for now", "user_token": "smoke-token" }),
        )
        .await
        .expect("defer with the correct user token must succeed");
    let dv: Value = serde_json::from_str(&text_of(&deferred)).expect("defer body JSON");
    assert_eq!(dv["status"].as_str(), Some("deferred"));

    let reinstated = server
        .call_tool_cli(
            "work_item_reinstate",
            json!({ "public_id": task_id, "reason": "back in scope", "user_token": "smoke-token" }),
        )
        .await
        .expect("reinstate must succeed");
    let rv2: Value = serde_json::from_str(&text_of(&reinstated)).expect("reinstate body JSON");
    assert_eq!(rv2["status"].as_str(), Some("in_progress"));
}

#[tokio::test]
async fn work_item_set_status_serializes_concurrent_transitions() {
    let db = require_test_db!();
    let server = server_1024(db.pool().clone());
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time after epoch")
        .as_nanos();

    let created = server
        .call_tool_cli(
            "work_item_create",
            json!({
                "title": format!("Concurrent status transition {suffix}"),
                "kind": "task",
                "body": "race regression",
            }),
        )
        .await
        .expect("create work item");
    let cv: Value = serde_json::from_str(&text_of(&created)).expect("create JSON");
    let item_id = cv["id"].as_i64().expect("work item id");

    let to_triage = pgmcp::db::queries::set_work_item_status(
        db.pool(),
        item_id,
        WorkItemStatus::Triage,
        Actor::Agent,
        Some("race-a"),
        Some("race to triage"),
        None,
        None,
    );
    let to_in_progress = pgmcp::db::queries::set_work_item_status(
        db.pool(),
        item_id,
        WorkItemStatus::InProgress,
        Actor::Agent,
        Some("race-b"),
        Some("race to progress"),
        None,
        None,
    );
    let (a, b) = tokio::join!(to_triage, to_in_progress);
    let successes = [a.as_ref().ok(), b.as_ref().ok()]
        .into_iter()
        .flatten()
        .count();
    assert_eq!(
        successes, 1,
        "racing pending -> triage and pending -> in_progress must not both commit"
    );

    let pending_history_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_item_status_history
         WHERE item_id = $1 AND from_status = 'pending'",
    )
    .bind(item_id)
    .fetch_one(db.pool())
    .await
    .expect("history count");
    assert_eq!(
        pending_history_count, 1,
        "exactly one transition may observe pending under the row lock"
    );
}

#[tokio::test]
async fn work_item_search_rejects_bad_scope_or_embedding_dimension() {
    let db = require_test_db!();
    let server = server_1024(db.pool().clone());

    assert!(
        server
            .call_tool_cli(
                "work_item_search",
                json!({ "query": "tracker", "project": "missing-project" }),
            )
            .await
            .is_err(),
        "an explicit unknown project must fail closed"
    );

    let bad_dim_server = server_with_embed_dim(db.pool().clone(), 8);
    assert!(
        bad_dim_server
            .call_tool_cli("work_item_search", json!({ "query": "tracker" }))
            .await
            .is_err(),
        "work_item_search rejects non-1024d query embeddings before pgvector"
    );
}

#[tokio::test]
async fn work_item_ingest_and_promote() {
    let db = require_test_db!();
    let server = server_1024(db.pool().clone());

    let md =
        "# Ship feature X\n## Build it\n### Wire the API\n- [ ] add endpoint\n- [x] add config\n";
    let ing = server
        .call_tool_cli("work_item_ingest_plan", json!({ "plan_markdown": md }))
        .await
        .expect("work_item_ingest_plan must succeed");
    let iv: Value = serde_json::from_str(&text_of(&ing)).expect("ingest body JSON");
    assert!(
        iv["created"].as_i64().unwrap_or(0) >= 4,
        "plan + epic + task + 2 todos created; got {}",
        iv["created"]
    );
    let root = iv["root_public_id"]
        .as_str()
        .expect("root_public_id")
        .to_string();

    // Idempotent re-ingest: creates nothing the second time.
    let ing2 = server
        .call_tool_cli("work_item_ingest_plan", json!({ "plan_markdown": md }))
        .await
        .expect("re-ingest must succeed");
    let iv2: Value = serde_json::from_str(&text_of(&ing2)).expect("re-ingest body JSON");
    assert_eq!(
        iv2["created"].as_i64(),
        Some(0),
        "re-ingesting the same plan is idempotent (no new items)"
    );

    // The ingested tree is fetchable.
    let tree = server
        .call_tool_cli("work_item_tree", json!({ "public_id": root }))
        .await
        .expect("tree must succeed");
    let tv: Value = serde_json::from_str(&text_of(&tree)).expect("tree body JSON");
    assert!(tv.as_array().map(|a| a.len() >= 4).unwrap_or(false));

    // Promote a code marker → a fixme item, idempotently.
    let m = server
        .call_tool_cli(
            "work_item_promote_marker",
            json!({ "marker_text": "FIXME: handle the edge case", "file": "src/x.rs", "line": 42 }),
        )
        .await
        .expect("work_item_promote_marker must succeed");
    let mv: Value = serde_json::from_str(&text_of(&m)).expect("promote body JSON");
    assert_eq!(
        mv["kind"].as_str(),
        Some("fixme"),
        "a FIXME marker becomes a fixme item"
    );

    let m2 = server
        .call_tool_cli(
            "work_item_promote_marker",
            json!({ "marker_text": "FIXME: handle the edge case", "file": "src/x.rs", "line": 42 }),
        )
        .await
        .expect("re-promote must succeed");
    let mv2: Value = serde_json::from_str(&text_of(&m2)).expect("re-promote body JSON");
    assert_eq!(
        mv2["already_promoted"].as_bool(),
        Some(true),
        "re-promoting the same marker is idempotent"
    );
}

#[tokio::test]
async fn work_item_ingest_plan_rejects_oversized_plan_before_writing() {
    let db = require_test_db!();
    let server = server_1024(db.pool().clone());

    let mut md = String::from("# Oversized ingest\n");
    for i in 0..501 {
        md.push_str(&format!("- [ ] oversized item {i}\n"));
    }

    assert!(
        server
            .call_tool_cli("work_item_ingest_plan", json!({ "plan_markdown": md }))
            .await
            .is_err(),
        "plans above the ingestion cap are rejected"
    );

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_items
         WHERE origin = 'ingest_plan' AND title = 'Oversized ingest'",
    )
    .fetch_one(db.pool())
    .await
    .expect("count oversized root");
    assert_eq!(count, 0, "oversized plans fail before any DB write");
}

#[tokio::test]
async fn work_item_ingest_plan_rolls_back_item_and_criteria_writes() {
    let db = require_test_db!();
    let server = server_1024(db.pool().clone());

    sqlx::query(
        "CREATE OR REPLACE FUNCTION pgmcp_testing_reject_boom_acceptance()
         RETURNS trigger
         LANGUAGE plpgsql
         AS $$
         BEGIN
             IF NEW.description = 'boom' THEN
                 RAISE EXCEPTION 'boom acceptance rejected for atomicity test';
             END IF;
             RETURN NEW;
         END
         $$",
    )
    .execute(db.pool())
    .await
    .expect("install trigger function");
    sqlx::query(
        "CREATE TRIGGER pgmcp_testing_reject_boom_acceptance
         BEFORE INSERT ON acceptance_criteria
         FOR EACH ROW
         EXECUTE FUNCTION pgmcp_testing_reject_boom_acceptance()",
    )
    .execute(db.pool())
    .await
    .expect("install trigger");

    let md =
        "# Atomic ingest\n- [ ] safe node\nacceptance: safe\n- [ ] doomed node\nacceptance: boom\n";
    assert!(
        server
            .call_tool_cli("work_item_ingest_plan", json!({ "plan_markdown": md }))
            .await
            .is_err(),
        "triggered criterion failure surfaces as a tool error"
    );

    let item_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_items
         WHERE origin = 'ingest_plan'
           AND title IN ('Atomic ingest', 'safe node', 'doomed node')",
    )
    .fetch_one(db.pool())
    .await
    .expect("count rolled-back items");
    assert_eq!(
        item_count, 0,
        "item upserts roll back with criterion failure"
    );

    let criterion_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM acceptance_criteria
         WHERE description IN ('safe', 'boom')",
    )
    .fetch_one(db.pool())
    .await
    .expect("count rolled-back criteria");
    assert_eq!(
        criterion_count, 0,
        "earlier criteria in the failed ingest are rolled back too"
    );
}

#[tokio::test]
async fn work_item_claim_concurrency_and_handoff() {
    let db = require_test_db!();
    let server = std::sync::Arc::new(server_1024(db.pool().clone()));

    server
        .call_tool_cli(
            "work_item_create",
            json!({ "kind": "task", "title": "contended", "public_id": "contended-item" }),
        )
        .await
        .expect("create the contended item");

    // Two agents race to claim the SAME item — exactly one must win.
    let (s1, s2) = (server.clone(), server.clone());
    let h1 = tokio::spawn(async move {
        s1.call_tool_cli(
            "work_item_claim",
            json!({ "public_id": "contended-item", "agent_id": "agent-a" }),
        )
        .await
    });
    let h2 = tokio::spawn(async move {
        s2.call_tool_cli(
            "work_item_claim",
            json!({ "public_id": "contended-item", "agent_id": "agent-b" }),
        )
        .await
    });
    let won = |r: Result<rmcp::model::CallToolResult, _>| -> bool {
        let r = r.expect("claim call ok");
        serde_json::from_str::<Value>(&text_of(&r)).unwrap()["claimed"]
            .as_bool()
            .unwrap_or(false)
    };
    let a = won(h1.await.expect("join a"));
    let b = won(h2.await.expect("join b"));
    assert!(a ^ b, "exactly one concurrent claimer wins (a={a}, b={b})");

    let winner = if a { "agent-a" } else { "agent-b" };

    // The loser cannot release.
    let loser = if a { "agent-b" } else { "agent-a" };
    assert!(
        server
            .call_tool_cli(
                "work_item_release",
                json!({ "public_id": "contended-item", "agent_id": loser }),
            )
            .await
            .is_err(),
        "a non-owner cannot release"
    );

    // The winner hands off to agent-c, who then releases.
    let ho = server
        .call_tool_cli(
            "work_item_handoff",
            json!({ "public_id": "contended-item", "to_agent": "agent-c", "agent_id": winner }),
        )
        .await
        .expect("owner can hand off");
    assert_eq!(
        serde_json::from_str::<Value>(&text_of(&ho)).unwrap()["handed_off_to"].as_str(),
        Some("agent-c")
    );
    server
        .call_tool_cli(
            "work_item_release",
            json!({ "public_id": "contended-item", "agent_id": "agent-c" }),
        )
        .await
        .expect("new owner can release");

    // claim_next picks a fresh ready item.
    server
        .call_tool_cli(
            "work_item_create",
            json!({ "kind": "task", "title": "next up", "public_id": "next-up" }),
        )
        .await
        .expect("create a claimable item");
    server
        .call_tool_cli(
            "work_item_set_status",
            json!({ "public_id": "next-up", "status": "ready" }),
        )
        .await
        .expect("mark ready");
    let nxt = server
        .call_tool_cli("work_item_claim_next", json!({ "agent_id": "agent-d" }))
        .await
        .expect("claim_next must succeed");
    assert_eq!(
        serde_json::from_str::<Value>(&text_of(&nxt)).unwrap()["claimed"].as_bool(),
        Some(true),
        "claim_next grabs the ready item"
    );
}

#[tokio::test]
async fn work_item_link_experiment_smoke() {
    let db = require_test_db!();
    let server = server_1024(db.pool().clone());

    // Seed a minimal experiment directly (the experiment subsystem owns the rich
    // data; the tracker only needs a row to link to).
    sqlx::query(
        "INSERT INTO experiments (slug, title, question) VALUES ($1, $2, $3)
         ON CONFLICT DO NOTHING",
    )
    .bind("smoke-exp")
    .bind("Smoke experiment")
    .bind("Does X improve Y?")
    .execute(db.pool())
    .await
    .expect("seed experiment row");

    // Link with no work_item_public_id ⇒ auto-create a kind='experiment' task.
    let linked = server
        .call_tool_cli(
            "work_item_link_experiment",
            json!({ "experiment_slug": "smoke-exp" }),
        )
        .await
        .expect("work_item_link_experiment must succeed");
    let lv: Value = serde_json::from_str(&text_of(&linked)).expect("link body JSON");
    assert_eq!(lv["linked"].as_bool(), Some(true));
    assert_eq!(
        lv["work_item_created"].as_bool(),
        Some(true),
        "auto-created the tracking task"
    );
    assert_eq!(lv["experiment_slug"].as_str(), Some("smoke-exp"));
    assert!(
        lv["criterion_id"].as_i64().is_some(),
        "seeded an experiment_verdict criterion"
    );

    // The created task is a kind='experiment' work item.
    let wid = lv["work_item_public_id"]
        .as_str()
        .expect("public_id")
        .to_string();
    let got = server
        .call_tool_cli("work_item_get", json!({ "public_id": wid }))
        .await
        .expect("get the tracking task");
    let gv: Value = serde_json::from_str(&text_of(&got)).expect("get body JSON");
    assert_eq!(gv["item"]["kind"].as_str(), Some("experiment"));

    // Re-link is idempotent (same task, no error).
    server
        .call_tool_cli(
            "work_item_link_experiment",
            json!({ "experiment_slug": "smoke-exp", "work_item_public_id": wid }),
        )
        .await
        .expect("idempotent re-link must succeed");

    // An unknown experiment slug is rejected.
    assert!(
        server
            .call_tool_cli(
                "work_item_link_experiment",
                json!({ "experiment_slug": "nope-zzz" })
            )
            .await
            .is_err(),
        "linking an unknown experiment must be rejected"
    );
}

#[tokio::test]
async fn work_item_phase9_relations_reporting() {
    let db = require_test_db!();
    let server = server_1024(db.pool().clone());

    // A plan with two task children.
    server
        .call_tool_cli(
            "work_item_create",
            json!({ "kind": "plan", "title": "P9 plan", "public_id": "p9-plan" }),
        )
        .await
        .expect("create plan");
    for (pid, title) in [("p9-a", "task A"), ("p9-b", "task B")] {
        server
            .call_tool_cli(
                "work_item_create",
                json!({ "kind": "task", "title": title, "public_id": pid, "parent_public_id": "p9-plan" }),
            )
            .await
            .expect("create child task");
    }

    // ── link + cycle guard ──
    let linked = server
        .call_tool_cli(
            "work_item_link",
            json!({ "from_public_id": "p9-a", "to_public_id": "p9-b", "relation_type": "depends_on" }),
        )
        .await
        .expect("work_item_link must succeed");
    assert_eq!(
        serde_json::from_str::<Value>(&text_of(&linked)).unwrap()["linked"].as_bool(),
        Some(true)
    );
    // The reverse depends_on closes a cycle → rejected.
    assert!(
        server
            .call_tool_cli(
                "work_item_link",
                json!({ "from_public_id": "p9-b", "to_public_id": "p9-a", "relation_type": "depends_on" }),
            )
            .await
            .is_err(),
        "a depends_on cycle must be rejected at link time"
    );

    // ── cycles report: the guard held, so the graph is a DAG ──
    let cyc = server
        .call_tool_cli("work_item_cycles", json!({ "plan_public_id": "p9-plan" }))
        .await
        .expect("work_item_cycles must succeed");
    assert_eq!(
        serde_json::from_str::<Value>(&text_of(&cyc)).unwrap()["is_dag"].as_bool(),
        Some(true),
        "no cycle exists because the reverse edge was rejected"
    );

    // ── unlink ──
    let unl = server
        .call_tool_cli(
            "work_item_unlink",
            json!({ "from_public_id": "p9-a", "to_public_id": "p9-b", "relation_type": "depends_on" }),
        )
        .await
        .expect("work_item_unlink must succeed");
    assert_eq!(
        serde_json::from_str::<Value>(&text_of(&unl)).unwrap()["removed"].as_bool(),
        Some(true)
    );

    // ── anchor_code: a path that matches no indexed file is a clean error ──
    assert!(
        server
            .call_tool_cli(
                "work_item_anchor_code",
                json!({ "public_id": "p9-a", "file": "does/not/exist/zzz.rs" }),
            )
            .await
            .is_err(),
        "anchoring to a non-indexed path must be rejected"
    );

    // ── plan definition TOML round-trip ──
    let toml_doc = "[definition]\nslug = \"p9-shape\"\nversion = 1\ntitle = \"P9 shape\"\nstatus = \"active\"\n\n[[rule]]\nrule_kind = \"required_child_kind\"\napplies_to_kind = \"plan\"\nchild_kind = \"task\"\nseverity = \"error\"\n";
    let imp = server
        .call_tool_cli("plan_definition_import", json!({ "toml": toml_doc }))
        .await
        .expect("plan_definition_import must succeed");
    let iv: Value = serde_json::from_str(&text_of(&imp)).expect("import body JSON");
    assert_eq!(iv["slug"].as_str(), Some("p9-shape"));
    assert_eq!(iv["rules"].as_i64(), Some(1));

    let exp = server
        .call_tool_cli("plan_definition_export", json!({ "slug": "p9-shape" }))
        .await
        .expect("plan_definition_export must succeed");
    let ev: Value = serde_json::from_str(&text_of(&exp)).expect("export body JSON");
    let toml_out = ev["toml"].as_str().expect("export returns TOML");
    assert!(
        toml_out.contains("[definition]") && toml_out.contains("p9-shape"),
        "exported TOML round-trips the definition"
    );
    assert_eq!(ev["rules"].as_i64(), Some(1), "the rule round-trips");

    // ── burndown: nothing verified yet ──
    let bd = server
        .call_tool_cli(
            "work_item_burndown",
            json!({ "plan_public_id": "p9-plan", "window_days": 7 }),
        )
        .await
        .expect("work_item_burndown must succeed");
    let bv: Value = serde_json::from_str(&text_of(&bd)).expect("burndown body JSON");
    assert!(
        bv["total"].as_i64().unwrap_or(0) >= 3,
        "plan + 2 tasks counted"
    );
    assert_eq!(bv["verified"].as_i64(), Some(0), "nothing verified yet");
    assert!(bv["remaining"].as_i64().unwrap_or(0) >= 3);

    // ── export (markdown) ──
    let md = server
        .call_tool_cli(
            "work_item_export",
            json!({ "plan_public_id": "p9-plan", "format": "markdown" }),
        )
        .await
        .expect("work_item_export must succeed");
    let mv: Value = serde_json::from_str(&text_of(&md)).expect("export body JSON");
    let content = mv["content"].as_str().expect("export returns content");
    assert!(
        content.contains("task A") && content.contains("task B") && content.contains("- ["),
        "markdown export renders the tasks as a checkbox list"
    );

    // ── export (org) ──
    let org = server
        .call_tool_cli(
            "work_item_export",
            json!({ "plan_public_id": "p9-plan", "format": "org" }),
        )
        .await
        .expect("work_item_export (org) must succeed");
    let ov: Value = serde_json::from_str(&text_of(&org)).expect("org body JSON");
    assert!(
        ov["content"].as_str().unwrap_or("").contains("TODO"),
        "org export renders TODO keywords"
    );
}

#[tokio::test]
async fn work_item_a2a_visibility() {
    let db = require_test_db!();
    let server = server_1024(db.pool().clone());

    // A claimable item owned by agent-vis.
    server
        .call_tool_cli(
            "work_item_create",
            json!({ "kind": "task", "title": "visible work", "public_id": "vis-item" }),
        )
        .await
        .expect("create the visible item");
    let claimed = server
        .call_tool_cli(
            "work_item_claim",
            json!({ "public_id": "vis-item", "agent_id": "agent-vis", "lease_secs": 300 }),
        )
        .await
        .expect("claim must succeed");
    assert_eq!(
        serde_json::from_str::<Value>(&text_of(&claimed)).unwrap()["claimed"].as_bool(),
        Some(true),
        "agent-vis claims vis-item"
    );

    // ── agent_heartbeat: marks the agent active + renews its held leases. ──
    let hb = server
        .call_tool_cli(
            "agent_heartbeat",
            json!({
                "agent_id": "agent-vis",
                "current_work_item_public_id": "vis-item",
                "lease_secs": 300,
            }),
        )
        .await
        .expect("agent_heartbeat must succeed");
    let hbv: Value = serde_json::from_str(&text_of(&hb)).expect("heartbeat body JSON");
    assert_eq!(hbv["agent_id"].as_str(), Some("agent-vis"));
    assert!(
        hbv["leases_renewed"].as_i64().unwrap_or(0) >= 1,
        "the heartbeat renews the lease on the held item"
    );

    // ── work_item_who_owns: agent-vis owns it; the claim history is non-empty. ──
    let owns = server
        .call_tool_cli("work_item_who_owns", json!({ "public_id": "vis-item" }))
        .await
        .expect("work_item_who_owns must succeed");
    let ov: Value = serde_json::from_str(&text_of(&owns)).expect("who_owns body JSON");
    assert_eq!(
        ov["owner"].as_str(),
        Some("agent-vis"),
        "agent-vis holds the item"
    );
    assert!(
        ov["history"]
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false),
        "the claim ledger has at least the claim event"
    );

    // ── agent_activity (scoped): what agent-vis is doing — its presence + load. ──
    let act = server
        .call_tool_cli("agent_activity", json!({ "agent_id": "agent-vis" }))
        .await
        .expect("agent_activity (scoped) must succeed");
    let av: Value = serde_json::from_str(&text_of(&act)).expect("activity body JSON");
    assert_eq!(av["agent_id"].as_str(), Some("agent-vis"));
    assert!(
        av["workload"].as_i64().unwrap_or(0) >= 1,
        "agent-vis is holding at least one item"
    );

    // ── agent_activity (roster): no agent_id ⇒ the active-agent roster. ──
    let roster = server
        .call_tool_cli("agent_activity", json!({ "active_within_secs": 3600 }))
        .await
        .expect("agent_activity (roster) must succeed");
    let rv: Value = serde_json::from_str(&text_of(&roster)).expect("roster body JSON");
    assert!(rv["roster"].is_array(), "the roster is an array");
    assert!(
        rv["roster"]
            .as_array()
            .map(|a| a
                .iter()
                .any(|r| r["agent_id"].as_str() == Some("agent-vis")))
            .unwrap_or(false),
        "agent-vis appears in the active roster after its heartbeat"
    );

    // ── work_item_activity: the workspace feed carries the claim/progress events. ──
    let feed = server
        .call_tool_cli("work_item_activity", json!({ "limit": 50 }))
        .await
        .expect("work_item_activity must succeed");
    let fv: Value = serde_json::from_str(&text_of(&feed)).expect("feed body JSON");
    assert!(fv["feed"].is_array(), "the activity feed is an array");
    assert!(
        fv["events"].as_i64().unwrap_or(0) >= 1,
        "the feed includes at least the claim event"
    );
}
