//! Integration test for Crucible session PAUSE/RESUME (ADR-009).
//!
//! Drives the three `session_checkpoint_*` MCP tools end-to-end through
//! `call_tool_cli` against a real test Postgres, covering:
//!   1. the linear happy path — synthesize a 2-worker protocol, execute one
//!      PlannedStep, save(paused), assert the trace was flushed to csm_run_traces,
//!      the work-item lease was dropped, and status='paused'; then resume and
//!      assert the returned `next_step` is the SECOND plan step (`plan()[1]`);
//!   2. the Critic-loop pause — a critic-gated protocol whose orchestrator faces
//!      the runtime Choice returns a `critic_verdict` await on resume;
//!   3. fork — resume(fork=true) copies the checkpoint into a fresh child session
//!      (parent_session_id set) and resumes the fork.
//!
//! These exercise pgmcp's persist/replay/validate boundary: every effect is a
//! read/write to pgmcp's OWN tables (orchestration_sessions / csm_run_traces /
//! work_items) — no shell, no user files.

mod common;

use std::future::Future;

use common::{server_with_pool, text_of};
use pgmcp::mcp::server::McpServer;
use pgmcp_testing::require_test_db;
use serde_json::{Value, json};
use sqlx::PgPool;

/// Run an async test body on a dedicated thread with a generous (32 MiB) stack.
///
/// The MCP tool dispatch future (`call_tool_cli` → the large `dispatch_named`
/// match → tool body → sqlx) is enormous in a debug build, and these tests hold
/// several deeply-nested JSON `Value`s (`global_type`, `transcript`) as locals
/// across many `.await` points. On the default libtest thread stack
/// (`RUST_MIN_STACK`, 2 MiB) that single `.await` chain overflows — a pure
/// debug-build harness limit, unrelated to the production daemon path (where the
/// MCP transport drives tools on tokio worker threads with a configured stack).
/// Polling the body on a 32 MiB current-thread runtime sidesteps it without
/// depending on the ambient `RUST_MIN_STACK`.
fn run_big_stack<F, Fut>(f: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()>,
{
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("current-thread runtime");
            rt.block_on(f());
        })
        .expect("spawn big-stack test thread")
        .join()
        .expect("big-stack test thread panicked");
}

/// Parse a tool result's text payload as JSON.
fn json_of(result: &rmcp::model::CallToolResult) -> Value {
    serde_json::from_str(&text_of(result)).expect("tool result is JSON")
}

/// Seed a plan work item (and clean any prior run), returning its public_id.
async fn seed_plan(pool: &PgPool, public_id: &str, n_tasks: usize) {
    sqlx::query("DELETE FROM work_items WHERE public_id LIKE $1")
        .bind(format!("{public_id}%"))
        .execute(pool)
        .await
        .expect("clean prior plan");
    let root_id: i64 = sqlx::query_scalar(
        "INSERT INTO work_items (public_id, kind, status, title)
         VALUES ($1, 'plan', 'pending', 'session checkpoint plan')
         RETURNING id",
    )
    .bind(public_id)
    .fetch_one(pool)
    .await
    .expect("seed plan root");
    for i in 0..n_tasks {
        sqlx::query(
            "INSERT INTO work_items (public_id, kind, status, title, parent_id)
             VALUES ($1, 'task', 'pending', $2, $3)",
        )
        .bind(format!("{public_id}-t{i}"))
        .bind(format!("task {i}"))
        .bind(root_id)
        .execute(pool)
        .await
        .expect("seed task");
    }
}

/// Synthesize a protocol from a seeded plan; returns the full tool envelope.
async fn synthesize(server: &McpServer, public_id: &str, critic: Option<&str>) -> Value {
    let mut args = json!({
        "public_id": public_id,
        "default_solver_agent": "code-generator",
    });
    if let Some(c) = critic {
        args["critic_agent"] = json!(c);
    }
    let res = server
        .call_tool_cli("csm_synthesize_protocol", args)
        .await
        .expect("csm_synthesize_protocol dispatches");
    json_of(&res)
}

/// Build the two-event trace (`O→peer:request`, `peer→O:response`) for one plan
/// step, as the JSON array the checkpoint transcript expects.
fn step_trace(step: &Value) -> Value {
    let peer = step["peer_role"].as_str().expect("peer_role");
    let req = step["request"].as_str().expect("request");
    let resp = step["response"].as_str().expect("response");
    json!([
        { "from": "O", "to": peer, "label": { "name": req } },
        { "from": peer, "to": "O", "label": { "name": resp } },
    ])
}

#[test]
fn linear_pause_then_resume_yields_next_plan_step() {
    run_big_stack(linear_pause_then_resume_yields_next_plan_step_body);
}

async fn linear_pause_then_resume_yields_next_plan_step_body() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let server = server_with_pool(pool.clone());

    let plan_id = "sess-lin-plan";
    seed_plan(&pool, plan_id, 2).await;

    // Synthesize a 2-worker linear protocol; capture its global_type + drivable plan.
    let synth = synthesize(&server, plan_id, None).await;
    assert_eq!(
        synth["drivable"],
        json!(true),
        "linear plan must be drivable"
    );
    let global_type = synth["global_type"].clone();
    let plan = synth["plan"].as_array().expect("a plan array").clone();
    assert!(
        plan.len() >= 2,
        "2-worker plan has >=2 steps, got {}",
        plan.len()
    );

    // Seed an a2a_task (no children → the pause guard passes) and claim the
    // work-item root for the orchestrator agent so the lease drop is observable.
    let task_id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO a2a_tasks (id, skill_id, status) VALUES (gen_random_uuid(), 'orchestration', 'working') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("seed a2a_task");
    let root_wi: i64 = sqlx::query_scalar("SELECT id FROM work_items WHERE public_id = $1")
        .bind(plan_id)
        .fetch_one(&pool)
        .await
        .expect("root work item id");
    let claimed = pgmcp::db::queries::claim_work_item(&pool, root_wi, "pi", 900)
        .await
        .expect("claim root");
    assert!(claimed.is_some(), "orchestrator claims the work-item root");

    // Execute PlannedStep 0 → a 1-step trace, then SAVE with pause=true.
    let session_key = "sess-lin-1";
    let trace = step_trace(&plan[0]);
    let save = server
        .call_tool_cli(
            "session_checkpoint_save",
            json!({
                "session_key": session_key,
                "protocol_name": format!("synthesized:{plan_id}"),
                "global_type": global_type,
                "task_id": task_id.to_string(),
                "cursor": 1,
                "role_peer": { "O": "pi", "W0": "code-generator", "W1": "code-generator" },
                "work_item_root": plan_id,
                "transcript": trace,
                "pause": true,
            }),
        )
        .await
        .expect("session_checkpoint_save dispatches");
    let save_json = json_of(&save);
    assert_eq!(
        save_json["paused"],
        json!(true),
        "pause must be granted (no live children)"
    );
    assert_eq!(save_json["status"], json!("paused"));
    assert_eq!(
        save_json["trace_events_flushed"],
        json!(2),
        "two events flushed"
    );
    assert_eq!(
        save_json["lease_dropped"],
        json!(true),
        "the work-item lease is dropped"
    );

    // The trace was persisted to csm_run_traces under the task_id.
    let trace_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM csm_run_traces WHERE task_id = $1")
            .bind(task_id)
            .fetch_one(&pool)
            .await
            .expect("count traces");
    assert_eq!(trace_rows, 1, "exactly one run-trace row flushed");

    // status='paused' on the row; lease cleared on the work item.
    let status: String =
        sqlx::query_scalar("SELECT status FROM orchestration_sessions WHERE session_key = $1")
            .bind(session_key)
            .fetch_one(&pool)
            .await
            .expect("session status");
    assert_eq!(status, "paused");
    let lease: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT lease_expires_at FROM work_items WHERE id = $1")
            .bind(root_wi)
            .fetch_one(&pool)
            .await
            .expect("lease");
    assert!(lease.is_none(), "work-item lease must be NULL after pause");

    // RESUME → the returned next_step must be plan()[1].
    let resume = server
        .call_tool_cli(
            "session_checkpoint_resume",
            json!({ "session_key": session_key }),
        )
        .await
        .expect("session_checkpoint_resume dispatches");
    let resume_json = json_of(&resume);
    assert_eq!(resume_json["conformant_prefix"], json!(true));
    assert_eq!(
        resume_json["status"],
        json!("running"),
        "resume flips status back to running"
    );
    assert_eq!(
        resume_json["replayed_events"],
        json!(2),
        "the 2-event prefix replayed"
    );
    let next = &resume_json["next_step"];
    assert!(
        !next.is_null(),
        "a next_step must be returned, got {resume_json}"
    );
    assert_eq!(
        next["request"], plan[1]["request"],
        "resume's next_step must match plan()[1]"
    );
    assert_eq!(next["peer_role"], plan[1]["peer_role"]);
    assert_eq!(
        resume_json["lease_reclaimed"],
        json!(true),
        "the lease is re-claimed on resume"
    );

    // The session is now listed as running, not resumable.
    let listed = server
        .call_tool_cli("session_checkpoint_list", json!({}))
        .await
        .expect("session_checkpoint_list dispatches");
    let listed_json = json_of(&listed);
    let any_match = listed_json["sessions"]
        .as_array()
        .map(|a| a.iter().any(|s| s["session_key"] == json!(session_key)))
        .unwrap_or(false);
    assert!(
        !any_match,
        "a running session must NOT appear in the resumable list"
    );
}

#[test]
fn critic_loop_pause_resume_awaits_verdict() {
    run_big_stack(critic_loop_pause_resume_awaits_verdict_body);
}

async fn critic_loop_pause_resume_awaits_verdict_body() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let server = server_with_pool(pool.clone());

    let plan_id = "sess-critic-plan";
    seed_plan(&pool, plan_id, 1).await;

    // Synthesize a Critic-gated protocol: NOT statically drivable (the orchestrator
    // faces a runtime Choice after the initial worker round).
    let synth = synthesize(&server, plan_id, Some("critic-bot")).await;
    assert_eq!(
        synth["drivable"],
        json!(false),
        "a critic-gated protocol is not statically drivable"
    );
    let global_type = synth["global_type"].clone();

    // Drive the initial worker round (O→W0:t0_req, W0→O:t0_done), then verify_req
    // (O→C:verify_req, C→O:?) — but the verdict is the runtime choice. We replay up
    // to the verify_req SEND only, which lands the orchestrator at the Choice.
    // The worker round + the verify_req send/recv pair would resolve the choice;
    // instead the executed trace is just the worker round, which leaves the
    // orchestrator at the verify_req send. We then take one more pair to reach the
    // Choice. The labels: worker round uses t0_req/t0_done; verify uses verify_req
    // and the verdict pass/revise. Resume after the worker round + verify send is
    // the Choice. Simplest deterministic prefix that reaches the Choice: the worker
    // round followed by O→C:verify_req and C→O:revise re-runs the workers — that is
    // NOT the choice state. To land AT the choice we stop right after verify_req is
    // delivered (the orchestrator has sent verify_req and awaits the verdict), i.e.
    // the trace ends with O→C:verify_req and C has received it but not replied.
    //
    // The orchestrator's state after sending verify_req (its Recv of the verdict is
    // the Choice) is where next_step_from returns None → critic await. The minimal
    // executed trace producing that state is: t0_req, t0_done, verify_req.
    let trace = json!([
        { "from": "O",  "to": "W0", "label": { "name": "t0_req" } },
        { "from": "W0", "to": "O",  "label": { "name": "t0_done" } },
        { "from": "O",  "to": "C",  "label": { "name": "verify_req" } },
    ]);

    let session_key = "sess-critic-1";
    // No task_id here → no flush/guard needed; the transcript carries the prefix.
    let save = server
        .call_tool_cli(
            "session_checkpoint_save",
            json!({
                "session_key": session_key,
                "protocol_name": format!("synthesized:{plan_id}"),
                "global_type": global_type,
                "critic_iteration": 1,
                "critic_phase": "awaiting_verdict",
                "role_peer": { "O": "pi", "W0": "code-generator", "C": "critic-bot" },
                "transcript": trace,
                "pause": true,
            }),
        )
        .await
        .expect("save dispatches");
    assert_eq!(json_of(&save)["paused"], json!(true));

    let resume = server
        .call_tool_cli(
            "session_checkpoint_resume",
            json!({ "session_key": session_key }),
        )
        .await
        .expect("resume dispatches");
    let resume_json = json_of(&resume);
    assert!(
        resume_json["next_step"].is_null(),
        "at the Choice there is no static next_step"
    );
    let choice = &resume_json["next_choice"];
    assert_eq!(
        choice["await"],
        json!("critic_verdict"),
        "must await the critic verdict, got {resume_json}"
    );
    assert_eq!(choice["critic_iteration"], json!(1));
    let branches = choice["branches"].as_array().expect("branches");
    assert!(
        branches.contains(&json!("pass")) && branches.contains(&json!("revise")),
        "both critic branches offered"
    );
}

#[test]
fn resume_fork_creates_child_session() {
    run_big_stack(resume_fork_creates_child_session_body);
}

async fn resume_fork_creates_child_session_body() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let server = server_with_pool(pool.clone());

    let plan_id = "sess-fork-plan";
    seed_plan(&pool, plan_id, 2).await;
    let synth = synthesize(&server, plan_id, None).await;
    let global_type = synth["global_type"].clone();
    let plan = synth["plan"].as_array().expect("plan").clone();

    // Pause a session with the first step executed (no task_id → transcript-only).
    let parent_key = "sess-fork-parent";
    let save = server
        .call_tool_cli(
            "session_checkpoint_save",
            json!({
                "session_key": parent_key,
                "protocol_name": format!("synthesized:{plan_id}"),
                "global_type": global_type,
                "cursor": 1,
                "role_peer": { "O": "pi", "W0": "code-generator", "W1": "code-generator" },
                "transcript": step_trace(&plan[0]),
                "pause": true,
            }),
        )
        .await
        .expect("save dispatches");
    assert_eq!(json_of(&save)["paused"], json!(true));

    // RESUME with fork=true → a fresh child session_key, parent_session_id set.
    let child_key = "sess-fork-child";
    let resume = server
        .call_tool_cli(
            "session_checkpoint_resume",
            json!({ "session_key": parent_key, "fork": true, "new_session_key": child_key }),
        )
        .await
        .expect("fork resume dispatches");
    let resume_json = json_of(&resume);
    assert_eq!(
        resume_json["session_key"],
        json!(child_key),
        "the fork is resumed under the new key"
    );
    assert_eq!(resume_json["forked_from"], json!(parent_key));
    // The fork's next_step is plan()[1] (it inherits the parent's executed prefix).
    assert_eq!(resume_json["next_step"]["request"], plan[1]["request"]);

    // The child row exists with parent_session_id pointing at the parent.
    let parent_id: i64 =
        sqlx::query_scalar("SELECT id FROM orchestration_sessions WHERE session_key = $1")
            .bind(parent_key)
            .fetch_one(&pool)
            .await
            .expect("parent id");
    let child_parent: Option<i64> = sqlx::query_scalar(
        "SELECT parent_session_id FROM orchestration_sessions WHERE session_key = $1",
    )
    .bind(child_key)
    .fetch_one(&pool)
    .await
    .expect("child parent");
    assert_eq!(child_parent, Some(parent_id), "the fork records its parent");

    // The original parent remains paused (the fork did not disturb it).
    let parent_status: String =
        sqlx::query_scalar("SELECT status FROM orchestration_sessions WHERE session_key = $1")
            .bind(parent_key)
            .fetch_one(&pool)
            .await
            .expect("parent status");
    assert_eq!(
        parent_status, "paused",
        "forking must not resume the parent"
    );
}
