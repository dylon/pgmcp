//! Real-Postgres integration test for the Phase-4 cross-project worktree
//! coordination tools and the gatekeeper trust boundary:
//!
//!   coordinate_dependency_block → coordination_respond → suggest_worktree
//!                                                      ↓ (git scanner / System)
//!                                              resolve_and_notify  ⇒ resolved
//!
//! It satisfies the `every_dispatched_tool_has_an_integration_test` coverage gate
//! for all three tools via the literal `call_tool_cli("…")`, AND asserts the
//! trust boundary proven in `docs/formal/WorktreeNegotiation.{tla,v}`: an editor
//! agent can drive a request to `moved` (a *candidate*) but can NEVER reach
//! `resolved` — only the git-scanner gatekeeper (`resolve_and_notify`, the
//! `Actor::System` path) may, and it then notifies the blocked requester.

use pgmcp_testing::pool_tool_helpers::{seed_project, server_with_pool};
use pgmcp_testing::require_test_db;

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

async fn status_of(pool: &sqlx::PgPool, req_id: i64) -> String {
    sqlx::query_scalar("SELECT status FROM coordination_requests WHERE id = $1")
        .bind(req_id)
        .fetch_one(pool)
        .await
        .expect("status fetch")
}

#[tokio::test(flavor = "multi_thread")]
async fn worktree_coordination_lifecycle_and_gatekeeper() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    // U = dependency (being edited); D = dependent (build broke on U).
    let u = seed_project(&pool, "u-crate", "/ws/u-crate").await;
    let _d = seed_project(&pool, "d-app", "/ws/d-app").await;
    let server = server_with_pool(pool.clone());

    // 1) D's agent: its build broke on dependency `u-crate` → open coordination.
    let res = server
        .call_tool_cli(
            "coordinate_dependency_block",
            serde_json::json!({
                "dependency": "u-crate",
                "dependent_project": "d-app",
                "error_excerpt": "error[E0432]: unresolved import `u_crate::NewApi`",
                "requester_session": "sess-D"
            }),
        )
        .await
        .expect("coordinate_dependency_block ok");
    let opened = tool_json(&res);
    let req_id = opened["request_id"].as_i64().expect("request_id");
    assert_eq!(opened["dependency"], "u-crate");
    assert_eq!(status_of(&pool, req_id).await, "pending");

    // The asserted dependency edge (compiler ground truth) was recorded D→U.
    let edge: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM project_dependencies
          WHERE dependent_project_id = (SELECT id FROM projects WHERE name='d-app')
            AND dependency_project_id = $1 AND source = 'asserted' AND valid_to IS NULL",
    )
    .bind(u)
    .fetch_one(&pool)
    .await
    .expect("edge count");
    assert_eq!(edge, 1, "compiler-asserted D→U edge recorded");

    // 2) U's editor responds `moved` — a CANDIDATE only, never `resolved`.
    let res = server
        .call_tool_cli(
            "coordination_respond",
            serde_json::json!({
                "request_id": req_id,
                "response": "moved",
                "editor_session": "sess-U",
                "worktree_branch": "feat/new-api"
            }),
        )
        .await
        .expect("coordination_respond ok");
    assert_eq!(tool_json(&res)["status"], "moved");
    assert_eq!(
        status_of(&pool, req_id).await,
        "moved",
        "agent 'moved' is a candidate — it must NOT reach resolved"
    );

    // TRUST BOUNDARY: an agent cannot set `resolved` — it is not even an
    // accepted response value (the tool rejects it before touching the DB).
    let bad = server
        .call_tool_cli(
            "coordination_respond",
            serde_json::json!({ "request_id": req_id, "response": "resolved" }),
        )
        .await;
    assert!(
        bad.is_err(),
        "an editor agent must not be able to drive a request to 'resolved'"
    );
    assert_eq!(
        status_of(&pool, req_id).await,
        "moved",
        "rejected self-resolve left the request a candidate"
    );

    // 3) U's editor calls suggest_worktree → git commands + the pending request
    //    (so it sees who is blocked and the id to answer).
    let res = server
        .call_tool_cli(
            "suggest_worktree",
            serde_json::json!({ "project": "u-crate", "feature_branch": "feat/new-api" }),
        )
        .await
        .expect("suggest_worktree ok");
    let sw = tool_json(&res);
    assert!(
        sw["commands"]
            .as_str()
            .unwrap_or_default()
            .contains("worktree add"),
        "suggests `git worktree add`"
    );
    assert_eq!(sw["pending_coordination_count"], 1);
    assert_eq!(sw["pending_coordinations"][0]["request_id"], req_id);

    // 4) GATEKEEPER: the git scanner observes `u-crate` back on its stable branch
    //    & clean → resolve_and_notify (the `Actor::System` path). This is the
    //    ONLY route to `resolved`.
    let resolved = pgmcp::deps::coord_store::resolve_and_notify(&pool, u)
        .await
        .expect("resolve_and_notify");
    assert_eq!(
        resolved,
        vec![req_id],
        "gatekeeper resolves the open request"
    );
    assert_eq!(
        status_of(&pool, req_id).await,
        "resolved",
        "only the git-scanner gatekeeper reaches resolved"
    );

    // 5) The blocked requester (D, sess-D) is notified it is unblocked.
    let res = server
        .call_tool_cli("a2a_inbox", serde_json::json!({ "session": "sess-D" }))
        .await
        .expect("a2a_inbox D ok");
    let inbox = tool_json(&res);
    let unblocked = inbox["messages"]
        .as_array()
        .map(|m| {
            m.iter().any(|msg| {
                msg["body"]
                    .as_str()
                    .map(|b| b.contains("unblocked"))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    assert!(unblocked, "requester received the unblock notification");
}

#[tokio::test(flavor = "multi_thread")]
async fn coordination_gates_and_system_unblocks_work_item() {
    // §4.5 close-the-loop: naming a blocked work-item sets it `blocked` on
    // coordinate, and the git-scanner gatekeeper (System) flips it `blocked →
    // ready` when the dependency is restored — automatically, no agent action.
    let db = require_test_db!();
    let pool = db.pool().clone();
    let u = seed_project(&pool, "wig-u", "/ws/wig-u").await;
    let _d = seed_project(&pool, "wig-d", "/ws/wig-d").await;
    let server = server_with_pool(pool.clone());

    // D's agent has a tracked task that the dependency blocks.
    let res = server
        .call_tool_cli(
            "work_item_create",
            serde_json::json!({ "kind": "task", "title": "ship feature on wig-d", "project": "wig-d" }),
        )
        .await
        .expect("work_item_create ok");
    let wi_public = tool_json(&res)["public_id"]
        .as_str()
        .expect("work-item public_id")
        .to_string();

    async fn wi_status(pool: &sqlx::PgPool, public_id: &str) -> String {
        sqlx::query_scalar("SELECT status FROM work_items WHERE public_id = $1")
            .bind(public_id)
            .fetch_one(pool)
            .await
            .expect("work-item status")
    }

    // Coordinate, naming the blocked work-item → it is set `blocked`.
    let res = server
        .call_tool_cli(
            "coordinate_dependency_block",
            serde_json::json!({
                "dependency": "wig-u",
                "dependent_project": "wig-d",
                "requester_session": "sess-WIG",
                "blocked_work_item": wi_public,
            }),
        )
        .await
        .expect("coordinate ok");
    let opened = tool_json(&res);
    let req_id = opened["request_id"].as_i64().expect("request_id");
    assert_eq!(opened["gated_work_item"], wi_public);
    assert_eq!(
        wi_status(&pool, &wi_public).await,
        "blocked",
        "the gated work-item is set blocked on coordinate"
    );

    // Gatekeeper: the dependency is restored → System auto-unblocks the work-item.
    let resolved = pgmcp::deps::coord_store::resolve_and_notify(&pool, u)
        .await
        .expect("resolve_and_notify");
    assert_eq!(resolved, vec![req_id]);
    assert_eq!(
        wi_status(&pool, &wi_public).await,
        "ready",
        "the git-scanner gatekeeper (System) auto-unblocked the work-item to ready"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn csm_validate_run_lifts_a_conforming_coordination_thread() {
    // §4.4: the typed mailbox thread (request_worktree . accept . moved) lifts into
    // a conforming WorktreeNegotiation trace, validated by `csm_validate_run` in
    // its coordination mode.
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _u = seed_project(&pool, "cv-u", "/ws/cv-u").await;
    let _d = seed_project(&pool, "cv-d", "/ws/cv-d").await;
    let server = server_with_pool(pool.clone());

    let res = server
        .call_tool_cli(
            "coordinate_dependency_block",
            serde_json::json!({
                "dependency": "cv-u",
                "dependent_project": "cv-d",
                "requester_session": "sess-CV"
            }),
        )
        .await
        .expect("coordinate ok");
    let req_id = tool_json(&res)["request_id"].as_i64().expect("request_id");

    // Editor accepts, then reports moved → a complete, conforming thread.
    for resp in ["accept", "moved"] {
        server
            .call_tool_cli(
                "coordination_respond",
                serde_json::json!({
                    "request_id": req_id,
                    "response": resp,
                    "editor_session": "sess-CVE",
                    "worktree_branch": "feat/x"
                }),
            )
            .await
            .expect("respond ok");
    }

    let res = server
        .call_tool_cli(
            "csm_validate_run",
            serde_json::json!({ "coordination_id": req_id }),
        )
        .await
        .expect("csm_validate_run coordination mode ok");
    let v = tool_json(&res);
    assert_eq!(v["protocol"], "worktree_negotiation");
    assert_eq!(
        v["conformant"], true,
        "accept→moved thread must conform: {v}"
    );
    assert!(
        v["n_events"].as_i64().unwrap_or(0) >= 3,
        "request_worktree + accept + moved lift to ≥3 events: {v}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn coordination_decline_is_terminal_for_the_agent() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let _u = seed_project(&pool, "u2-crate", "/ws/u2-crate").await;
    let _d = seed_project(&pool, "d2-app", "/ws/d2-app").await;
    let server = server_with_pool(pool.clone());

    let res = server
        .call_tool_cli(
            "coordinate_dependency_block",
            serde_json::json!({
                "dependency": "u2-crate",
                "dependent_project": "d2-app",
                "requester_session": "sess-D2"
            }),
        )
        .await
        .expect("coordinate ok");
    let req_id = tool_json(&res)["request_id"].as_i64().expect("request_id");

    // Editor declines — a complete, legal run; the dependent escalates/withdraws.
    let res = server
        .call_tool_cli(
            "coordination_respond",
            serde_json::json!({ "request_id": req_id, "response": "decline" }),
        )
        .await
        .expect("decline ok");
    assert_eq!(tool_json(&res)["status"], "declined");
    assert_eq!(status_of(&pool, req_id).await, "declined");
}
