//! Boy-Scout coverage-gate tests for the profiling/debugging tools merged from
//! crucible-pgmcp that arrived without a `call_tool_cli` integration test
//! (profile_ingest, runtime_deadlock_reconcile, trace_map_to_code). Each is
//! driven through the dispatcher against a seeded project; minimal trace input
//! exercises the parse + resolve path and must return gracefully.

mod common;

use common::{server_with_pool, text_of};
use pgmcp_testing::require_test_db;
use serde_json::json;

fn body(r: &rmcp::model::CallToolResult) -> serde_json::Value {
    serde_json::from_str(&text_of(r)).expect("tool body must be JSON")
}

async fn seed_project(pool: &sqlx::PgPool) {
    sqlx::query(
        "INSERT INTO projects (workspace_path, path, name) VALUES ('/ws/prof','/ws/prof/p','profproj')
         ON CONFLICT (path) DO UPDATE SET name='profproj'",
    )
    .execute(pool)
    .await
    .expect("project");
}

#[tokio::test]
async fn profile_ingest_dispatches() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let server = server_with_pool(pool.clone());
    seed_project(&pool).await;

    let res = body(
        &server
            .call_tool_cli(
                "profile_ingest",
                json!({"project": "profproj", "content": "main 100\nfoo 40", "kind": "perf"}),
            )
            .await
            .expect("profile_ingest"),
    );
    assert!(res.is_object(), "profile_ingest returns an object: {res}");
}

#[tokio::test]
async fn runtime_deadlock_reconcile_dispatches() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let server = server_with_pool(pool.clone());
    seed_project(&pool).await;

    let res = body(
        &server
            .call_tool_cli(
                "runtime_deadlock_reconcile",
                json!({"project": "profproj", "trace_text": "main;lock_a;lock_b 1", "format": "offcpu_folded"}),
            )
            .await
            .expect("runtime_deadlock_reconcile"),
    );
    assert!(res.is_object(), "{res}");
}

#[tokio::test]
async fn trace_map_to_code_dispatches() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let server = server_with_pool(pool.clone());
    seed_project(&pool).await;

    let res = body(
        &server
            .call_tool_cli(
                "trace_map_to_code",
                json!({"project": "profproj", "backtrace": "main\nfoo\nbar", "format": "folded"}),
            )
            .await
            .expect("trace_map_to_code"),
    );
    assert!(res.is_object(), "{res}");
}
