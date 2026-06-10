//! Fail-closed correctness oracle for the hardened coordination/client/A2A tool
//! family (the `8c6e62b` boundary-hardening recipe applied to
//! `coordinate_dependency_block`, `project_dependents`, `project_dependencies`,
//! `suggest_worktree`, `a2a_send_message`). Asserts the boundary obligations the
//! `docs/formal/coordination-tools-traceability.md` ledger + `tla/*Scope.tla`
//! slices model: blank input rejected, DUPLICATE display names fail closed
//! (`project_id_or_err`), unknown projects rejected, blank message body rejected.
//! Complements the success-path lifecycle tests in `tool_coordination.rs`.

use pgmcp_testing::pool_tool_helpers::{seed_project, server_with_pool};
use pgmcp_testing::require_test_db;

/// Seed two projects sharing one display name — the ambiguity `project_id_or_err`
/// must reject. Returns the shared name.
async fn seed_duplicate_named<'a>(pool: &sqlx::PgPool, name: &'a str) -> &'a str {
    seed_project(pool, name, &format!("/ws/{name}-a")).await;
    seed_project(pool, name, &format!("/ws/{name}-b")).await;
    name
}

#[tokio::test(flavor = "multi_thread")]
async fn coordinate_dependency_block_rejects_blank_dup_and_unknown() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let dup = seed_duplicate_named(&pool, "ocrd-dupdep").await;
    let _ok = seed_project(&pool, "ocrd-real", "/ws/ocrd-real").await;
    let server = server_with_pool(pool.clone());

    // Duplicate dependency name → fail closed (ambiguous), no coordination opened.
    assert!(
        server
            .call_tool_cli(
                "coordinate_dependency_block",
                serde_json::json!({ "dependency": dup })
            )
            .await
            .is_err(),
        "duplicate dependency name must fail closed"
    );
    // Blank dependency → rejected.
    assert!(
        server
            .call_tool_cli(
                "coordinate_dependency_block",
                serde_json::json!({ "dependency": "   " })
            )
            .await
            .is_err(),
        "blank dependency must be rejected"
    );
    // Unknown dependency → rejected.
    assert!(
        server
            .call_tool_cli(
                "coordinate_dependency_block",
                serde_json::json!({ "dependency": "ocrd-nonexistent" })
            )
            .await
            .is_err(),
        "unknown dependency must be rejected"
    );
    // A valid dependent_project that is DUPLICATE must also fail closed.
    assert!(
        server
            .call_tool_cli(
                "coordinate_dependency_block",
                serde_json::json!({ "dependency": "ocrd-real", "dependent_project": dup })
            )
            .await
            .is_err(),
        "duplicate dependent_project must fail closed"
    );

    // No coordination_requests row was created by any rejected call.
    let opened: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM coordination_requests")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(opened, 0, "rejected requests must not open a coordination");
}

#[tokio::test(flavor = "multi_thread")]
async fn project_deps_tools_fail_closed_on_blank_and_duplicate() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let dup = seed_duplicate_named(&pool, "ocrd-dupproj").await;
    let server = server_with_pool(pool.clone());

    for tool in ["project_dependents", "project_dependencies"] {
        assert!(
            server
                .call_tool_cli(tool, serde_json::json!({ "project": dup }))
                .await
                .is_err(),
            "{tool}: duplicate project name must fail closed"
        );
        assert!(
            server
                .call_tool_cli(tool, serde_json::json!({ "project": "  " }))
                .await
                .is_err(),
            "{tool}: blank project must be rejected"
        );
        assert!(
            server
                .call_tool_cli(tool, serde_json::json!({ "project": "ocrd-missing" }))
                .await
                .is_err(),
            "{tool}: unknown project must be rejected"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn suggest_worktree_fail_closed_on_blank_and_duplicate() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let dup = seed_duplicate_named(&pool, "ocrd-dupwt").await;
    let server = server_with_pool(pool.clone());

    assert!(
        server
            .call_tool_cli("suggest_worktree", serde_json::json!({ "project": dup }))
            .await
            .is_err(),
        "suggest_worktree: duplicate project must fail closed (cannot pick an arbitrary row)"
    );
    assert!(
        server
            .call_tool_cli("suggest_worktree", serde_json::json!({ "project": "" }))
            .await
            .is_err(),
        "suggest_worktree: blank project must be rejected"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a2a_send_message_rejects_blank_body() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let server = server_with_pool(pool.clone());

    // Blank body → rejected even with a valid address.
    assert!(
        server
            .call_tool_cli(
                "a2a_send_message",
                serde_json::json!({ "to_session": "sess-X", "body": "   " })
            )
            .await
            .is_err(),
        "a2a_send_message: blank body must be rejected"
    );
}
