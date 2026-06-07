//! Real-Postgres correctness oracle for the shadow-ASR concurrency tools
//! (`deadlock_cycles`, `channel_deadlock`, `sync_skeleton`).
//!
//! The unit tests in `src/graph/{lock_order,petri}.rs` prove the *algorithms*;
//! this oracle proves the *DB wiring* end-to-end: seeded `sync_ops` +
//! `symbol_references` rows flow through `sync_skeleton_for_project` /
//! `resolved_call_edges_for_project` into the analyzers and out through the MCP
//! tool envelope. It asserts both *detection* (an interprocedural A→B / B→A lock
//! cycle and a mutual-receive channel cycle surface) and *no false positive* (a
//! same-order lock fixture yields zero cycles).
//!
//! `require_test_db!` skips cleanly when no test DB is configured, so this runs
//! inside `verify.sh` Gate 5 without an `#[ignore]`.

use pgmcp_testing::pool_tool_helpers::{
    seed_file, seed_file_symbol, seed_project, seed_symbol_references, seed_sync_ops,
    server_with_pool,
};
use pgmcp_testing::require_test_db;

/// Extract the first text block of a tool result as JSON (the
/// `pool_tool_helpers` tests don't pull in the `common` module's `text_of`).
fn tool_json(result: &rmcp::model::CallToolResult) -> serde_json::Value {
    let text = result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .next()
        .expect("text content present");
    serde_json::from_str(&text).expect("tool output is JSON")
}

/// Seed an interprocedural A→B / B→A lock cycle and a mutual-receive channel
/// cycle into one project, then assert both `deadlock_cycles` and
/// `channel_deadlock` detect them.
#[tokio::test(flavor = "multi_thread")]
async fn deadlock_cycles_and_channel_cycle_are_detected() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let project = seed_project(&pool, "conc-deadlock", "/ws/conc-deadlock").await;
    let file = seed_file(&pool, project, "/ws/conc-deadlock/f.rs", "f.rs").await;

    // ── Interprocedural lock cycle ──────────────────────────────────────────
    // worker_one: acquire(lock_alpha); call acquire_beta()  → edge alpha→beta
    // worker_two: acquire(lock_beta);  call acquire_alpha() → edge beta→alpha
    // The call edges live in symbol_references (the inliner synthesizes the
    // held-while-call ordering); the leaf acquires live in the callees.
    let worker_one = seed_file_symbol(&pool, file, "worker_one", "function", 1, None).await;
    let acquire_beta = seed_file_symbol(&pool, file, "acquire_beta", "function", 10, None).await;
    let worker_two = seed_file_symbol(&pool, file, "worker_two", "function", 20, None).await;
    let acquire_alpha = seed_file_symbol(&pool, file, "acquire_alpha", "function", 30, None).await;

    seed_sync_ops(
        &pool,
        worker_one,
        0,
        "acquire",
        "lock_alpha",
        "mutex",
        "lock",
        2,
    )
    .await;
    seed_sync_ops(
        &pool,
        acquire_beta,
        0,
        "acquire",
        "lock_beta",
        "mutex",
        "lock",
        11,
    )
    .await;
    seed_sync_ops(
        &pool,
        worker_two,
        0,
        "acquire",
        "lock_beta",
        "mutex",
        "lock",
        21,
    )
    .await;
    seed_sync_ops(
        &pool,
        acquire_alpha,
        0,
        "acquire",
        "lock_alpha",
        "mutex",
        "lock",
        31,
    )
    .await;

    // Call edges (source_line after the acquire line so the held-set carries the
    // outer lock into the call); confidence 0.9 ≥ the 0.5 inliner floor.
    seed_symbol_references(
        &pool,
        file,
        worker_one,
        acquire_beta,
        "acquire_beta",
        3,
        0.9,
    )
    .await;
    seed_symbol_references(
        &pool,
        file,
        worker_two,
        acquire_alpha,
        "acquire_alpha",
        22,
        0.9,
    )
    .await;

    // ── Channel (message) cycle ─────────────────────────────────────────────
    // proc_one: recv(chan_x); send(chan_y)   proc_two: recv(chan_y); send(chan_x)
    // Each blocks on its first receive, which only the other produces → cycle.
    let proc_one = seed_file_symbol(&pool, file, "proc_one", "function", 40, None).await;
    let proc_two = seed_file_symbol(&pool, file, "proc_two", "function", 50, None).await;
    seed_sync_ops(
        &pool, proc_one, 0, "recv", "chan_x", "channel", "message", 41,
    )
    .await;
    seed_sync_ops(
        &pool, proc_one, 1, "send", "chan_y", "channel", "message", 42,
    )
    .await;
    seed_sync_ops(
        &pool, proc_two, 0, "recv", "chan_y", "channel", "message", 51,
    )
    .await;
    seed_sync_ops(
        &pool, proc_two, 1, "send", "chan_x", "channel", "message", 52,
    )
    .await;

    let server = server_with_pool(pool);

    // deadlock_cycles → the {lock_alpha, lock_beta} interprocedural cycle.
    let result = server
        .call_tool_cli(
            "deadlock_cycles",
            serde_json::json!({"project": "conc-deadlock"}),
        )
        .await
        .expect("deadlock_cycles call");
    let v = tool_json(&result);
    let cycles = v["deadlock_cycles"]
        .as_array()
        .expect("deadlock_cycles array");
    assert!(
        !cycles.is_empty(),
        "expected the interprocedural AB/BA cycle, got none: {v}"
    );
    let matched = cycles.iter().find(|c| {
        let mut resources: Vec<&str> = c["resources"]
            .as_array()
            .map_or_else(Vec::new, |a| a.iter().filter_map(|r| r.as_str()).collect());
        resources.sort_unstable();
        resources == ["lock_alpha", "lock_beta"]
    });
    let matched = matched.expect("a cycle over exactly {lock_alpha, lock_beta}");
    assert!(
        matched["edges"]
            .as_array()
            .expect("edges")
            .iter()
            .any(|e| e["interprocedural"].as_bool() == Some(true)),
        "the cycle must be witnessed by interprocedural (callee-inlined) edges: {matched}"
    );

    // channel_deadlock → the proc_one/proc_two communication cycle.
    let result = server
        .call_tool_cli(
            "channel_deadlock",
            serde_json::json!({"project": "conc-deadlock"}),
        )
        .await
        .expect("channel_deadlock call");
    let v = tool_json(&result);
    let findings = v["findings"].as_array().expect("findings array");
    assert!(
        findings
            .iter()
            .any(|f| f["finding_kind"].as_str() == Some("channel_cycle")),
        "expected a channel_cycle finding, got: {v}"
    );
}

/// A same-order lock fixture (both symbols acquire alpha→beta) must yield no
/// cycle, and `sync_skeleton` must report the ordered held-set.
#[tokio::test(flavor = "multi_thread")]
async fn same_order_locks_have_no_cycle_and_sync_skeleton_reports_held_set() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let project = seed_project(&pool, "conc-safe", "/ws/conc-safe").await;
    let file = seed_file(&pool, project, "/ws/conc-safe/s.rs", "s.rs").await;

    // Both functions acquire in the SAME order (alpha then beta) → acyclic.
    let safe_one = seed_file_symbol(&pool, file, "safe_one", "function", 1, None).await;
    let safe_two = seed_file_symbol(&pool, file, "safe_two", "function", 10, None).await;
    seed_sync_ops(
        &pool,
        safe_one,
        0,
        "acquire",
        "lock_alpha",
        "mutex",
        "lock",
        2,
    )
    .await;
    seed_sync_ops(
        &pool,
        safe_one,
        1,
        "acquire",
        "lock_beta",
        "mutex",
        "lock",
        3,
    )
    .await;
    seed_sync_ops(
        &pool,
        safe_two,
        0,
        "acquire",
        "lock_alpha",
        "mutex",
        "lock",
        11,
    )
    .await;
    seed_sync_ops(
        &pool,
        safe_two,
        1,
        "acquire",
        "lock_beta",
        "mutex",
        "lock",
        12,
    )
    .await;

    let server = server_with_pool(pool);

    // No false positive: zero cycles.
    let result = server
        .call_tool_cli(
            "deadlock_cycles",
            serde_json::json!({
                "project": " conc-safe ",
                "confidence_floor": 5.0,
                "max_call_depth": 99,
                "max_cycle_len": 1,
                "limit": -20,
            }),
        )
        .await
        .expect("deadlock_cycles call");
    let v = tool_json(&result);
    assert_eq!(v["project"].as_str(), Some("conc-safe"));
    assert_eq!(v["limit"].as_u64(), Some(1));
    assert_eq!(v["max_call_depth"].as_u64(), Some(12));
    assert_eq!(v["confidence_floor"].as_f64(), Some(1.0));
    assert_eq!(
        v["deadlock_cycles"].as_array().map(Vec::len),
        Some(0),
        "same-order acquisition must not produce a cycle: {v}"
    );

    // sync_skeleton drill-down: safe_one holds {alpha} then {alpha, beta}.
    let result = server
        .call_tool_cli(
            "sync_skeleton",
            serde_json::json!({"project": "conc-safe", "symbol_id": safe_one}),
        )
        .await
        .expect("sync_skeleton call");
    let v = tool_json(&result);
    assert_eq!(v["op_count"].as_i64(), Some(2), "two ops: {v}");
    let ops = v["ops"].as_array().expect("ops array");
    let held_after_last: Vec<&str> = ops[1]["held_after"]
        .as_array()
        .expect("held_after")
        .iter()
        .filter_map(|h| h.as_str())
        .collect();
    assert!(
        held_after_last.contains(&"lock_alpha") && held_after_last.contains(&"lock_beta"),
        "after the second acquire both locks are held: {held_after_last:?}"
    );
}

/// The remaining CLI-dispatched concurrency tools (`lock_order_graph`,
/// `concurrency_bottlenecks`, `concurrency_forecast`) must dispatch and run
/// against a seeded skeleton. `lock_order_graph`/`concurrency_bottlenecks` read
/// `sync_ops`; `concurrency_forecast` reads `concurrency_health_history` (empty
/// here, so it degrades to a zero-sample forecast with an explanatory note).
#[tokio::test(flavor = "multi_thread")]
async fn lock_graph_bottlenecks_and_forecast_tools_dispatch() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let project = seed_project(&pool, "conc-inspect", "/ws/conc-inspect").await;
    let file = seed_file(&pool, project, "/ws/conc-inspect/i.rs", "i.rs").await;

    // A direct intra-procedural AB/BA cycle so the lock-order graph is non-empty.
    let one = seed_file_symbol(&pool, file, "one", "function", 1, None).await;
    let two = seed_file_symbol(&pool, file, "two", "function", 10, None).await;
    seed_sync_ops(&pool, one, 0, "acquire", "lock_alpha", "mutex", "lock", 2).await;
    seed_sync_ops(&pool, one, 1, "acquire", "lock_beta", "mutex", "lock", 3).await;
    seed_sync_ops(&pool, two, 0, "acquire", "lock_beta", "mutex", "lock", 11).await;
    seed_sync_ops(&pool, two, 1, "acquire", "lock_alpha", "mutex", "lock", 12).await;

    let server = server_with_pool(pool);

    // lock_order_graph + concurrency_bottlenecks: dispatch and run cleanly.
    // NB: string literals (not a loop variable) so the static coverage gate
    // `every_dispatched_tool_has_an_integration_test` can see each tool name.
    let r = server
        .call_tool_cli(
            "lock_order_graph",
            serde_json::json!({
                "project": " conc-inspect ",
                "confidence_floor": -7.5,
                "max_call_depth": 0,
            }),
        )
        .await
        .expect("lock_order_graph call");
    assert!(
        r.is_error != Some(true),
        "lock_order_graph returned an error envelope"
    );
    let v = tool_json(&r);
    assert_eq!(v["project"].as_str(), Some("conc-inspect"));
    assert_eq!(v["max_call_depth"].as_u64(), Some(1));
    assert_eq!(v["confidence_floor"].as_f64(), Some(0.0));
    let r = server
        .call_tool_cli(
            "concurrency_bottlenecks",
            serde_json::json!({"project": "conc-inspect"}),
        )
        .await
        .expect("concurrency_bottlenecks call");
    assert!(
        r.is_error != Some(true),
        "concurrency_bottlenecks returned an error envelope"
    );

    // concurrency_forecast: no health snapshots → graceful zero-sample forecast.
    let r = server
        .call_tool_cli(
            "concurrency_forecast",
            serde_json::json!({"project": "conc-inspect", "metric": "deadlock_cycle_count"}),
        )
        .await
        .expect("concurrency_forecast call");
    assert!(r.is_error != Some(true), "concurrency_forecast errored");
    let v = tool_json(&r);
    assert_eq!(v["metric"].as_str(), Some("deadlock_cycle_count"));
    assert_eq!(
        v["sample_count"].as_i64(),
        Some(0),
        "no seeded history → zero samples: {v}"
    );
    assert!(
        v["note"]
            .as_str()
            .is_some_and(|n| n.contains("insufficient history")),
        "expected an insufficient-history note: {v}"
    );
}
