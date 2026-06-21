//! Integration test for the Crucible **working-set resume** wiring (Phase 6):
//! the context-tape paging control plane's residency snapshot survives a
//! PAUSE/RESUME round-trip bit-identically — "the trace IS the position".
//!
//! Two layers, both against a real test Postgres:
//!
//!   1. The DETERMINISM CORE (engine + store, no MCP): drive the P5
//!      [`PagingEngine`] over a [`MockTapeDataPlane`] to page in three pages,
//!      `flush_working_set` (the PAUSE flush), then `load_working_set` (the
//!      RESUME rehydrate). Assert the reconstructed [`WorkingSet`] has the
//!      IDENTICAL three resident addresses, the IDENTICAL `resident_tokens`, and
//!      the IDENTICAL `last_access_ord` ordering — the logical-clock determinism
//!      guarantee (`last_access_ord` is a logical clock value, never wall-time).
//!
//!   2. The MCP LIFECYCLE (mirrors `session_checkpoint_lifecycle.rs`): with a
//!      working set persisted at `(session_key, cursor)`, drive
//!      `session_checkpoint_save(pause=true)` and assert it reports
//!      `working_set_pages_flushed == 3`; then `session_checkpoint_resume` and
//!      assert the result's `working_set.resident_pages == 3` (the rehydrated
//!      residency summary).
//!
//! Every effect is a read/write to pgmcp's OWN tables (working_set_pages /
//! working_set_config / orchestration_sessions / csm_run_traces) plus the mock
//! data plane — no shell, no user files.

mod common;

use std::future::Future;

use common::{server_with_pool, text_of};
use pgmcp::tape::data_plane::{MockTapeDataPlane, PageQuery, TreePath};
use pgmcp::tape::engine::PagingEngine;
use pgmcp::tape::store;
use pgmcp::tape::vocab::{EvictionPolicy, PageKind};
use pgmcp::tape::working_set::{PageAddr, WorkingSet};
use pgmcp_testing::require_test_db;
use serde_json::{Value, json};
use sqlx::PgPool;

/// Run an async test body on a dedicated 32 MiB-stack thread (the MCP
/// `call_tool_cli` dispatch future overflows the 2 MiB libtest stack in debug).
/// Identical to `session_checkpoint_lifecycle.rs`'s helper.
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

/// Insert the parent `orchestration_sessions` row (FK target for the working-set
/// tables) for `session_key`, cleaning any prior rows first. The empty
/// `global_type` `{}` is a placeholder; the MCP-lifecycle test overwrites the row
/// with a real GlobalType via `session_checkpoint_save`.
async fn seed_session(pool: &PgPool, session_key: &str) {
    sqlx::query("DELETE FROM orchestration_sessions WHERE session_key = $1")
        .bind(session_key)
        .execute(pool)
        .await
        .expect("clean prior session");
    sqlx::query(
        "INSERT INTO orchestration_sessions (session_key, protocol_name, global_type)
         VALUES ($1, 'tape-resume-test', '{}'::jsonb)",
    )
    .bind(session_key)
    .execute(pool)
    .await
    .expect("seed orchestration_sessions");
}

/// Drive the paging engine to page in exactly THREE pages (each fits the budget)
/// under `(session_key, cursor)`, returning the in-memory working set the engine
/// produced. The pages are deterministically named `pg-a` / `pg-b` / `pg-c` with
/// distinct importance + token costs so the reload order is observable.
async fn page_in_three(
    pool: &PgPool,
    dp: &MockTapeDataPlane,
    tree: &TreePath,
    session_key: &str,
    cursor: i32,
) -> WorkingSet {
    // Budget 1000 comfortably holds all three (Σ = 30+50+20 = 100 tokens).
    dp.insert_page(
        tree,
        &PageAddr("pg-a".into()),
        "alpha",
        30,
        0.9,
        PageKind::FileChunk,
    );
    dp.insert_page(
        tree,
        &PageAddr("pg-b".into()),
        "bravo",
        50,
        0.5,
        PageKind::FileChunk,
    );
    dp.insert_page(
        tree,
        &PageAddr("pg-c".into()),
        "charlie",
        20,
        0.1,
        PageKind::FileChunk,
    );

    let engine = PagingEngine::new(pool, dp);
    let mut ws = WorkingSet::new(
        session_key,
        cursor,
        1000,
        EvictionPolicy::ImportanceWeighted,
    );
    let outcome = engine
        .page_in(
            &mut ws,
            tree,
            &PageQuery::Semantic {
                query: "q".into(),
                k: 10,
            },
        )
        .await
        .expect("page_in three pages");
    assert_eq!(outcome.admitted.len(), 3, "all three pages admitted");
    assert_eq!(ws.pages.len(), 3, "three resident pages");
    ws
}

/// A snapshot of the working set's observable residency: ordered (addr,
/// last_access_ord, est_tokens) plus the resident-token sum. The determinism
/// guarantee is that a save→load round-trip preserves this exactly.
fn snapshot(ws: &WorkingSet) -> (Vec<(String, u64, i32)>, i32) {
    let order = ws
        .pages
        .iter_in_order()
        .map(|p| (p.addr.0.clone(), p.last_access_ord, p.est_tokens))
        .collect();
    (order, ws.resident_tokens)
}

// ---------------------------------------------------------------------------
// 1. The determinism core: page-in → flush (pause) → load (resume) is identical.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn working_set_survives_pause_resume_bit_identically() {
    let db = require_test_db!();
    let pool = db.pool();
    let session = "tape-resume-core";
    let cursor = 0;
    seed_session(pool, session).await;

    let dp = MockTapeDataPlane::new();
    let tree = TreePath::for_root_task("t-resume-core");

    // Page in three pages (the engine persists incrementally as it admits).
    let ws = page_in_three(pool, &dp, &tree, session, cursor).await;
    let (order_before, tokens_before) = snapshot(&ws);
    assert_eq!(tokens_before, 100, "Σ est_tokens = 30+50+20");
    let logical_clock_before = ws.clock;

    // The three resident addresses (the identity we must preserve).
    let addrs_before: std::collections::BTreeSet<String> =
        ws.pages.iter_in_order().map(|p| p.addr.0.clone()).collect();
    assert_eq!(
        addrs_before,
        ["pg-a", "pg-b", "pg-c"]
            .into_iter()
            .map(String::from)
            .collect()
    );

    // PAUSE: flush the durable working set at the suspend point. This is the
    // function the `session_checkpoint_save(pause)` tool calls; it re-commits the
    // residency state and reports the resident-page count.
    let flushed = store::flush_working_set(pool, session, cursor)
        .await
        .expect("flush working set at pause");
    assert_eq!(flushed, 3, "all three resident pages flushed");

    // RESUME: rehydrate the working set from the persisted logical metadata. This
    // is the function the `session_checkpoint_resume` tool calls.
    let reloaded = store::load_working_set(pool, session, cursor)
        .await
        .expect("load working set on resume");

    // (a) IDENTICAL three resident addresses.
    let addrs_after: std::collections::BTreeSet<String> = reloaded
        .pages
        .iter_in_order()
        .map(|p| p.addr.0.clone())
        .collect();
    assert_eq!(
        addrs_after, addrs_before,
        "the resumed working set has the identical three resident addresses"
    );

    // (b) IDENTICAL resident_tokens.
    assert_eq!(
        reloaded.resident_tokens, tokens_before,
        "resident_tokens is reconstructed exactly (Σ est_tokens)"
    );
    assert_eq!(
        reloaded.resident_tokens,
        reloaded.recompute_resident_tokens(),
        "the reloaded token sum is self-consistent"
    );

    // (c) IDENTICAL last_access_ord ORDERING (the logical-clock determinism
    //     guarantee). The reload orders by (last_access_ord, addr); the
    //     pre-pause in-memory set is in admission (logical-clock) order, and the
    //     engine stamps last_access_ord monotonically per admission, so the two
    //     orderings — and every (addr, ord, tokens) triple — coincide.
    let (order_after, _) = snapshot(&reloaded);
    assert_eq!(
        order_after, order_before,
        "the (addr, last_access_ord, est_tokens) sequence is bit-identical across resume"
    );
    // The last_access_ord values are strictly increasing across the three pages
    // (a logical clock, not wall-time): pg-a < pg-b < pg-c in admission order.
    let ords: Vec<u64> = order_after.iter().map(|(_, ord, _)| *ord).collect();
    assert!(
        ords.windows(2).all(|w| w[0] < w[1]),
        "last_access_ord is a strictly-monotonic logical clock: {ords:?}"
    );

    // (d) The config (logical clock / policy / budget) round-trips too.
    assert_eq!(
        reloaded.clock, logical_clock_before,
        "logical clock preserved"
    );
    assert_eq!(reloaded.policy, EvictionPolicy::ImportanceWeighted);
    assert_eq!(reloaded.budget_tokens, 1000);

    // (e) A SECOND flush+load is still identical (idempotent, non-destructive).
    let flushed2 = store::flush_working_set(pool, session, cursor)
        .await
        .expect("second flush");
    assert_eq!(flushed2, 3, "second flush still reports three pages");
    let reloaded2 = store::load_working_set(pool, session, cursor)
        .await
        .expect("second load");
    let (order_after2, tokens_after2) = snapshot(&reloaded2);
    assert_eq!(
        order_after2, order_before,
        "second resume identical to the first"
    );
    assert_eq!(tokens_after2, tokens_before);
}

// ---------------------------------------------------------------------------
// 2. flush_working_set on a session with NO working set is a benign no-op.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn flush_with_no_working_set_is_zero_noop() {
    let db = require_test_db!();
    let pool = db.pool();
    let session = "tape-resume-empty";
    seed_session(pool, session).await;

    // No pages ever paged in for this session → flush is a 0-page no-op (does not
    // error), so a paused session that never used the tape pays nothing.
    let flushed = store::flush_working_set(pool, session, 0)
        .await
        .expect("flush of an empty working set must not error");
    assert_eq!(flushed, 0, "no working set ⇒ zero pages flushed");

    // load_working_set likewise returns an empty set (zero budget default).
    let ws = store::load_working_set(pool, session, 0)
        .await
        .expect("load empty");
    assert_eq!(ws.pages.len(), 0);
    assert_eq!(ws.resident_tokens, 0);
}

// ---------------------------------------------------------------------------
// 3. The MCP lifecycle: save(pause) flushes the working set, resume rehydrates it
//    (mirrors session_checkpoint_lifecycle.rs, driven through call_tool_cli).
// ---------------------------------------------------------------------------
#[test]
fn mcp_pause_flushes_and_resume_rehydrates_working_set() {
    run_big_stack(mcp_pause_flushes_and_resume_rehydrates_working_set_body);
}

async fn mcp_pause_flushes_and_resume_rehydrates_working_set_body() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let server = server_with_pool(pool.clone());

    let session = "tape-resume-mcp";
    let cursor: i32 = 1;
    seed_session(&pool, session).await;

    // Stand up a working set at (session, cursor) by driving the engine.
    let dp = MockTapeDataPlane::new();
    let tree = TreePath::for_root_task("t-resume-mcp");
    let _ws = page_in_three(&pool, &dp, &tree, session, cursor).await;

    // A minimal real GlobalType the checkpoint can rebuild + replay: a one-step
    // linear protocol O→W0:t0_req . W0→O:t0_done . end, with the one-step prefix
    // already executed (so resume lands the orchestrator terminal → done=true).
    // The point under test is the working-set rehydrate, not the next step.
    let global_type = json!({
        "type": "interaction",
        "data": {
            "from": "O",
            "to": "W0",
            "label": { "name": "t0_req" },
            "cont": {
                "type": "interaction",
                "data": {
                    "from": "W0",
                    "to": "O",
                    "label": { "name": "t0_done" },
                    "cont": { "type": "end" }
                }
            }
        }
    });
    let transcript = json!([
        { "from": "O",  "to": "W0", "label": { "name": "t0_req" } },
        { "from": "W0", "to": "O",  "label": { "name": "t0_done" } },
    ]);

    // SAVE with pause=true → the working set is flushed at the suspend point.
    // (No task_id → the transcript carries the prefix; no pause guard needed.)
    let save = server
        .call_tool_cli(
            "session_checkpoint_save",
            json!({
                "session_key": session,
                "protocol_name": "tape-resume-mcp",
                "global_type": global_type,
                "cursor": cursor,
                "role_peer": { "O": "pi", "W0": "code-generator" },
                "transcript": transcript,
                "pause": true,
            }),
        )
        .await
        .expect("session_checkpoint_save dispatches");
    let save_json = json_of(&save);
    assert_eq!(save_json["paused"], json!(true), "pause granted");
    assert_eq!(
        save_json["working_set_pages_flushed"],
        json!(3),
        "the three resident pages are flushed at pause, got {save_json}"
    );

    // RESUME → the rehydrated working-set summary reports three resident pages.
    let resume = server
        .call_tool_cli(
            "session_checkpoint_resume",
            json!({ "session_key": session }),
        )
        .await
        .expect("session_checkpoint_resume dispatches");
    let resume_json = json_of(&resume);
    assert_eq!(resume_json["conformant_prefix"], json!(true));
    let ws_summary = &resume_json["working_set"];
    assert_eq!(
        ws_summary["resident_pages"],
        json!(3),
        "resume rehydrates the three-page working set, got {resume_json}"
    );
    assert_eq!(
        ws_summary["resident_tokens"],
        json!(100),
        "resident_tokens reconstructed (30+50+20)"
    );
    assert_eq!(ws_summary["budget_tokens"], json!(1000));
    assert_eq!(ws_summary["policy"], json!("importance_weighted"));
}
