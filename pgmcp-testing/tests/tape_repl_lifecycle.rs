//! Cross-crate lifecycle tests for the **`tape_repl`** MCP tool — the white-box /
//! latent-tier sandboxed REPL behind the structural admission gate (Phase 8,
//! Unit C).
//!
//! Scenarios:
//!   1. `tape_repl_refuses_black_box_via_call_tool_cli` — the gate refuses at the
//!      `call_tool_cli("tape_repl", …)` surface (no live DB ⇒ the named experiment
//!      cannot be confirmed Open ⇒ fail-closed refusal). This invocation is
//!      ALSO required by `query_inventory_vs_coverage`'s inventory guard so the new
//!      dispatch arm has coverage — without it the static guard fails.
//!   2. `tape_repl_white_box_open_experiment_runs_and_writes_scratch` — with a live
//!      Postgres and an **Open** experiment, the admitted run executes a `put`+`get`
//!      script, returns the value, writes a tree-local Scratch page, and leaves the
//!      durable corpus untouched.
//!   3. `tape_repl_over_limit_returns_structured_over_limit` — a budget-busting
//!      script yields a structured `over_limit: true`, NOT a transport 500.
//!   4. `tape_repl_caller_role_threading_gate` — the **wire-level trust boundary**
//!      (the SECURITY fix): the body parameterized by the host-extracted caller
//!      identity REFUSES a lowercased black-box `"claude"` caller even with an
//!      **Open** experiment (this assertion FAILS on the pre-fix code, which fed the
//!      gate a constant white-box `"Orchestrator"` role for every caller), ADMITS a
//!      positively-identified white-box `"reflector"` caller, and fails closed for
//!      an unknown / empty identity. A DB-free companion
//!      (`tape_repl_unknown_caller_fails_closed_no_db`) proves the unidentified
//!      caller is refused without any experiment at all.
//!
//! Tests 2 & 3 self-skip (via `require_test_db!`) when `PGMCP_TEST_DATABASE_URL`
//! is unset, so they stay green for contributors without local Postgres while
//! still being source-grepped by the inventory guard.
//!
//! ## Why the chained body runs on a `with_big_stack` thread
//!
//! `McpServer::call_tool_cli` returns a single future whose size is the maximum
//! over every arm of the ~330-tool `dispatch_tool!` match. Several such futures
//! constructed across the awaits of one `async fn` can exceed a test thread's
//! default 2 MiB stack and overflow — a *test-harness* artifact (in production
//! each request runs one verb on its own task stack). The chained tests run on a
//! dedicated 16 MiB-stack thread, mirroring `tape_verbs.rs`.

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
use pgmcp_testing::mocks::{DeterministicEmbeddingBackend, MockDbClient};
use pgmcp_testing::require_test_db;
use serde_json::{Value, json};
use sqlx::PgPool;

/// Parse a tool result's text payload as JSON.
fn json_of(result: &rmcp::model::CallToolResult) -> Value {
    serde_json::from_str(&text_of(result)).expect("tool body must be JSON")
}

/// Run an async test body on a dedicated 16 MiB-stack thread with its own
/// current-thread Tokio runtime (see the module docs).
fn with_big_stack<F>(body: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build current-thread runtime")
                .block_on(body);
        })
        .expect("spawn big-stack test thread")
        .join()
        .expect("big-stack test thread panicked");
}

/// Await one tool-call future and return its parsed JSON body.
async fn call(
    fut: impl std::future::Future<Output = Result<rmcp::model::CallToolResult, rmcp::ErrorData>>,
) -> Value {
    json_of(&fut.await.expect("tool call must succeed"))
}

/// A fresh, unique tree id per test so the per-tree stores never collide.
fn fresh_tree() -> String {
    format!("tape-repl-{}", uuid::Uuid::new_v4())
}

/// A `SystemContext` over the given `DbClient`, with a 1024-d deterministic
/// embedder and a Ready lifecycle. Shared by the `McpServer` builders and the
/// caller-role threading test (which drives `tool_tape_repl_with_caller`
/// directly, so it needs the `SystemContext` — `McpServer::ctx()` is
/// `pub(crate)` and not reachable from this external test crate).
fn ctx_over(db: Arc<dyn DbClient>) -> SystemContext {
    let stats = Arc::new(StatsTracker::new());
    let config = Arc::new(ArcSwap::from_pointee(Config::default()));
    let log_broadcaster = Arc::new(LogBroadcaster::new());
    let task_store = Arc::new(TaskStore::new());
    let embed_backend: Arc<dyn pgmcp::embed::EmbeddingBackend> =
        Arc::new(DeterministicEmbeddingBackend::new(1024));
    let embed_source = EmbedSource::backend(embed_backend);
    let lifecycle = pgmcp::daemon_state::DaemonLifecycle::new();
    lifecycle.transition(pgmcp::daemon_state::DaemonPhase::Ready);
    SystemContext::production(
        db,
        embed_source,
        stats,
        config,
        log_broadcaster,
        task_store,
        lifecycle,
    )
}

/// A `SystemContext` with no real DB — sufficient for the refusal-surface tests.
fn mock_ctx() -> SystemContext {
    ctx_over(Arc::new(MockDbClient::new()))
}

/// A `SystemContext` over a real pool + the 1024-d deterministic embedder
/// (matches the experiment tables' `vector(1024)` columns so embed-on-write does
/// not dimension-mismatch).
fn ctx_1024(pool: PgPool) -> SystemContext {
    ctx_over(Arc::new(pool))
}

/// A server with no real DB — sufficient for the refusal-surface test.
fn mock_server() -> McpServer {
    McpServer::new(mock_ctx())
}

/// Server with a real pool + a 1024-d deterministic embedder (matches the
/// experiment tables' `vector(1024)` columns so embed-on-write does not
/// dimension-mismatch).
fn server_1024(pool: PgPool) -> McpServer {
    McpServer::new(ctx_1024(pool))
}

/// Open an experiment via the real `experiment_open` tool and return its slug.
/// The new experiment defaults to status `open`, which is what `tape_repl`'s
/// admission gate requires.
async fn open_experiment(server: &McpServer, title: &str) -> String {
    let open = server
        .call_tool_cli(
            "experiment_open",
            json!({
                "title": title,
                "question": "Does the white-box REPL admit under an open experiment?",
                "context": "tape_repl lifecycle fixture.",
                "kind": "investigation",
                "hypothesis": "An open experiment admits the REPL",
                "primary_metric": "admitted",
                "unit": "bool",
                "lower_is_better": false,
            }),
        )
        .await
        .expect("experiment_open must succeed");
    let ov: Value = serde_json::from_str(&text_of(&open)).expect("open body JSON");
    ov["slug"]
        .as_str()
        .expect("experiment_open returns a slug")
        .to_string()
}

// ===========================================================================
// 1. Refusal at the dispatch surface (required by the inventory guard).
//    No live DB ⇒ the named experiment cannot be confirmed Open ⇒ refused.
// ===========================================================================
#[test]
fn tape_repl_refuses_black_box_via_call_tool_cli() {
    with_big_stack(async {
        let server = mock_server();
        let tree = fresh_tree();
        let v = call(server.call_tool_cli(
            "tape_repl",
            json!({
                "tree": tree,
                "script": r#"put("scratch/x/01", "nope")"#,
                "experiment_slug": "any-slug",
            }),
        ))
        .await;
        // The gate refuses: with no live pool the experiment cannot be confirmed
        // Open, so admission fails closed (a by-design refusal, not a 500).
        assert_eq!(
            v["admitted"], false,
            "tape_repl must refuse when admission cannot be granted; got {v}"
        );
        assert!(
            v["reason"].as_str().is_some_and(|r| !r.is_empty()),
            "a refusal must carry a non-empty structural reason; got {v}"
        );
    });
}

// ===========================================================================
// 2. Admitted path: white-box + Open experiment runs a put+get and writes
//    Scratch; the corpus is never written.
// ===========================================================================
#[test]
fn tape_repl_white_box_open_experiment_runs_and_writes_scratch() {
    with_big_stack(async {
        let db = require_test_db!();
        let server = server_1024(db.pool().clone());
        let tree = fresh_tree();

        // Seed an OPEN experiment; admission requires its status == open.
        let slug = open_experiment(&server, "tape_repl admitted run").await;

        // Read this tree's UUID from tape_stat so the Scratch path is well-formed
        // (PageAddress::to_path == scratch/<tree-uuid>/<hex-slot>).
        let tid = call(server.call_tool_cli("tape_stat", json!({"tree": tree}))).await["tree_id"]
            .as_str()
            .expect("tape_stat returns the tree_id")
            .to_string();
        let address = format!("scratch/{tid}/abcd");

        // A put+get DSL script: write a Scratch page, then read it back.
        let script = format!(r#"put("{address}", "written by repl"); get("{address}")"#);
        let v = call(server.call_tool_cli(
            "tape_repl",
            json!({ "tree": tree, "script": script, "experiment_slug": slug }),
        ))
        .await;

        assert_eq!(
            v["admitted"], true,
            "white-box + open experiment must admit; got {v}"
        );
        assert_eq!(
            v["value"], "written by repl",
            "the get verb must return the written Scratch body; got {v}"
        );
        assert_eq!(
            v["over_limit"], false,
            "a small script does not bust the budget"
        );
        assert!(v["error"].is_null(), "a clean run has no error; got {v}");

        // The Scratch page is now resident (one dirty page) — proof the REPL wrote
        // through to the per-tree store.
        let stat = call(server.call_tool_cli("tape_stat", json!({"tree": tree}))).await;
        assert_eq!(
            stat["n_pages"].as_u64(),
            Some(1),
            "exactly one resident page"
        );
        assert_eq!(
            stat["n_dirty"].as_u64(),
            Some(1),
            "the Scratch write is dirty"
        );

        // And it is enumerable under the scratch/ prefix — but NEVER a corpus page
        // (the REPL's put is Scratch-only; the corpus is read-only).
        let list = call(server.call_tool_cli("tape_list", json!({"tree": tree}))).await;
        let addrs = list["addresses"].as_array().expect("addresses array");
        assert!(
            addrs.iter().any(|a| a == &Value::String(address.clone())),
            "the written address must be listed; got {addrs:?}"
        );
        assert!(
            addrs
                .iter()
                .all(|a| a.as_str().is_some_and(|s| s.starts_with("scratch/"))),
            "every written page must be Scratch — the corpus is never written; got {addrs:?}"
        );
    });
}

// ===========================================================================
// 3. Over-limit: a budget-busting script returns over_limit:true, not a 500.
// ===========================================================================
#[test]
fn tape_repl_over_limit_returns_structured_over_limit() {
    with_big_stack(async {
        let db = require_test_db!();
        let server = server_1024(db.pool().clone());
        let tree = fresh_tree();
        let slug = open_experiment(&server, "tape_repl over-limit run").await;

        // A tiny operation budget forces an abort on a busy loop — the abort is a
        // STRUCTURED outcome (over_limit:true), not a transport error.
        let script = "let s = 0; let i = 0; while i < 100000 { s = s + i; i = i + 1; } s";
        let v = call(server.call_tool_cli(
            "tape_repl",
            json!({
                "tree": tree,
                "script": script,
                "experiment_slug": slug,
                "limits": { "max_operations": 50 },
            }),
        ))
        .await;

        assert_eq!(
            v["admitted"], true,
            "the run is admitted (the abort is post-admission)"
        );
        assert_eq!(
            v["over_limit"], true,
            "a budget-busting script must report over_limit:true; got {v}"
        );
        assert_eq!(
            v["limit"], "operations",
            "the tripped budget must be the operation ceiling; got {v}"
        );
        assert!(
            v["error"].is_null(),
            "an over-limit abort is not an error; got {v}"
        );
    });
}

// ===========================================================================
// 4. The wire-level trust boundary (the SECURITY fix).
//
//    The body parameterized by the host-extracted caller identity
//    (`tool_tape_repl_with_caller`, the exact seam the MCP wire handler uses,
//    threading `extract_caller(&ctx).client_name`) must REFUSE a lowercased
//    black-box "claude" caller — even with an Open experiment — and ADMIT a
//    positively-identified white-box "reflector" caller.
//
//    This is the assertion that bites the defect: pre-fix, the body constructed
//    `Role::new("Orchestrator")` for EVERY caller, which is absent from the
//    (then TitleCase) black-box set, so an Open experiment alone admitted any
//    caller. With the fix, the real lowercased identity is threaded and compared
//    against the lowercase canonical set, so "claude" is structurally refused on
//    the latent edge.
// ===========================================================================

/// Build typed `TapeReplParams` from a JSON body (mirrors how `dispatch_tool!`
/// deserializes the wire params), for a direct `tool_tape_repl_with_caller` call.
fn repl_params(tree: &str, slug: &str) -> pgmcp::mcp::server::TapeReplParams {
    serde_json::from_value(json!({
        "tree": tree,
        "script": r#"put("scratch/x/01", "nope")"#,
        "experiment_slug": slug,
    }))
    .expect("TapeReplParams must deserialize")
}

#[test]
fn tape_repl_caller_role_threading_gate() {
    with_big_stack(async {
        let db = require_test_db!();
        // Keep the `SystemContext` so the direct `tool_tape_repl_with_caller` calls
        // can use it (`McpServer::ctx()` is `pub(crate)`, unreachable here); the
        // `McpServer` (a clone of the same ctx) drives `experiment_open`.
        let ctx = ctx_1024(db.pool().clone());
        let server = McpServer::new(ctx.clone());

        // An OPEN experiment so the experiment arm passes — isolating the MEDIUM
        // (caller-role) arm as the sole decider of admission.
        let slug = open_experiment(&server, "tape_repl caller-role threading").await;

        // (a) Lowercased BLACK-BOX "claude" + Open experiment ⇒ REFUSED on the
        //     latent edge. This is the line that FAILS on the pre-fix code.
        let tree = fresh_tree();
        let claude = json_of(
            &pgmcp::mcp::tools::tool_tape_repl::tool_tape_repl_with_caller(
                &ctx,
                repl_params(&tree, &slug),
                Some("claude"),
            )
            .await
            .expect("tool call must succeed (a refusal is a structured Ok, not an Err)"),
        );
        assert_eq!(
            claude["admitted"], false,
            "a lowercased black-box 'claude' caller must be REFUSED even with an Open \
             experiment (this asserts the fail-OPEN defect is closed); got {claude}"
        );
        let reason = claude["reason"]
            .as_str()
            .expect("a refusal carries a structural reason");
        assert!(
            reason.contains("latent") || reason.contains("black-box"),
            "the refusal must cite the latent / black-box medium boundary (NOT the \
             experiment status), proving the medium arm bit; got: {reason}"
        );

        // (b) Positively-identified WHITE-BOX "reflector" + Open experiment ⇒ ADMITTED.
        let tree = fresh_tree();
        let reflector = json_of(
            &pgmcp::mcp::tools::tool_tape_repl::tool_tape_repl_with_caller(
                &ctx,
                repl_params(&tree, &slug),
                Some("reflector"),
            )
            .await
            .expect("tool call must succeed"),
        );
        assert_eq!(
            reflector["admitted"], true,
            "a white-box backbone caller (absent from black_box_roles) + Open \
             experiment must be ADMITTED; got {reflector}"
        );

        // (c) UNKNOWN / empty identity ⇒ REFUSED (fail-closed) even with the Open
        //     experiment — the unidentified caller is mapped onto a black-box role.
        for ident in [Some("unknown"), Some(""), None] {
            let tree = fresh_tree();
            let v = json_of(
                &pgmcp::mcp::tools::tool_tape_repl::tool_tape_repl_with_caller(
                    &ctx,
                    repl_params(&tree, &slug),
                    ident,
                )
                .await
                .expect("tool call must succeed"),
            );
            assert_eq!(
                v["admitted"], false,
                "an unidentified caller {ident:?} must fail CLOSED (refused) even with \
                 an Open experiment; got {v}"
            );
        }
    });
}

// ===========================================================================
// 4b. DB-free companion: an unidentified caller is refused without any
//     experiment at all (the medium arm refuses first, before the experiment
//     arm is even consulted). Runs even without local Postgres.
// ===========================================================================
#[test]
fn tape_repl_unknown_caller_fails_closed_no_db() {
    with_big_stack(async {
        let ctx = mock_ctx();
        let tree = fresh_tree();
        let v = json_of(
            &pgmcp::mcp::tools::tool_tape_repl::tool_tape_repl_with_caller(
                &ctx,
                repl_params(&tree, "any-slug"),
                Some("unknown"),
            )
            .await
            .expect("tool call must succeed (refusal is a structured Ok)"),
        );
        assert_eq!(
            v["admitted"], false,
            "an unidentified caller must fail closed on the medium arm with no DB; got {v}"
        );
        assert!(
            v["reason"].as_str().is_some_and(|r| !r.is_empty()),
            "a refusal carries a non-empty structural reason; got {v}"
        );
    });
}
