//! Direct store-level tests for the Phase-4 dependency + coordination stores:
//! the `coord_store` respond/resolve transitions (the agent-vs-gatekeeper trust
//! boundary at the store layer, below the MCP tools) and `store::close_stale`
//! (bitemporal edge closing that retains history).

use pgmcp::deps::DepSource;
use pgmcp::deps::coordination::CoordinationStatus;
use pgmcp::deps::{coord_store as cs, store};
use pgmcp_testing::pool_tool_helpers::seed_project;
use pgmcp_testing::require_test_db;

#[tokio::test(flavor = "multi_thread")]
async fn coord_store_respond_transitions_and_resolve_gatekeeper() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let u = seed_project(&pool, "ds-u", "/ws/ds-u").await;
    let d = seed_project(&pool, "ds-d", "/ws/ds-d").await;

    let req = cs::open_request(
        &pool,
        Some(d),
        u,
        Some("agent-D"),
        Some("sess-D"),
        Some("build broke"),
        Some("E0432"),
        None,
    )
    .await
    .expect("open_request");

    // accept + moved are agent-settable (return true / one row updated).
    assert!(
        cs::respond(
            &pool,
            req,
            CoordinationStatus::Accepted,
            Some("sess-U"),
            None
        )
        .await
        .expect("accept"),
        "accept is agent-settable"
    );
    assert!(
        cs::respond(
            &pool,
            req,
            CoordinationStatus::Moved,
            Some("sess-U"),
            Some("feat/x")
        )
        .await
        .expect("moved"),
        "moved is agent-settable (a candidate)"
    );

    // `resolved` is NOT agent-settable — respond refuses it (returns false, no
    // DB change). This is the store-level trust boundary.
    assert!(
        !cs::respond(
            &pool,
            req,
            CoordinationStatus::Resolved,
            Some("sess-U"),
            None
        )
        .await
        .expect("resolve-refused"),
        "an agent cannot set 'resolved' via respond()"
    );
    let status: String =
        sqlx::query_scalar("SELECT status FROM coordination_requests WHERE id = $1")
            .bind(req)
            .fetch_one(&pool)
            .await
            .expect("status");
    assert_eq!(
        status, "moved",
        "the refused self-resolve left it a candidate"
    );

    // The git-scanner gatekeeper (System) is the ONLY path to resolved.
    let resolved = cs::resolve_on_stable(&pool, u)
        .await
        .expect("resolve_on_stable");
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].id, req);
    assert_eq!(resolved[0].dependent_project_id, Some(d));
    let status2: String =
        sqlx::query_scalar("SELECT status FROM coordination_requests WHERE id = $1")
            .bind(req)
            .fetch_one(&pool)
            .await
            .expect("status2");
    assert_eq!(status2, "resolved", "only the gatekeeper reaches resolved");

    // A resolved request is no longer "open" for the editor's view.
    let open = cs::open_for_dependency(&pool, u)
        .await
        .expect("open_for_dependency");
    assert!(open.is_empty(), "resolved request is not in the open set");

    // respond on a terminal (resolved) request is a no-op (false).
    assert!(
        !cs::respond(&pool, req, CoordinationStatus::Accepted, None, None)
            .await
            .expect("respond-after-resolved"),
        "no transition out of a resolved request"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn store_close_stale_closes_vanished_edges_keeping_history() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let u = seed_project(&pool, "cs-u", "/ws/cs-u").await;
    let d = seed_project(&pool, "cs-d", "/ws/cs-d").await;

    // A live cargo edge D→U.
    store::upsert_dependency(&pool, d, u, Some("u"), Some("path"), DepSource::Cargo, 1.0)
        .await
        .expect("upsert");
    assert_eq!(
        store::dependencies_of(&pool, d).await.expect("deps").len(),
        1,
        "the edge is live"
    );
    assert_eq!(
        store::dependents_of(&pool, u).await.expect("rev").len(),
        1,
        "and visible from the reverse side"
    );

    // Close edges last seen before a FUTURE cutoff (i.e. all of them) — the
    // manifest no longer declares the dependency.
    let future = chrono::Utc::now() + chrono::Duration::days(1);
    let closed = store::close_stale(&pool, d, DepSource::Cargo, future)
        .await
        .expect("close_stale");
    assert_eq!(closed, 1, "the vanished cargo edge is closed");

    // The live forward query no longer returns it...
    assert!(
        store::dependencies_of(&pool, d)
            .await
            .expect("deps2")
            .is_empty(),
        "closed edge is no longer live"
    );
    // ...but the row is retained (valid_to set), so history is preserved.
    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM project_dependencies WHERE dependent_project_id = $1",
    )
    .bind(d)
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(total, 1, "closed edge is kept as bitemporal history");

    // A re-appearance reopens a fresh live edge (upsert after close).
    store::upsert_dependency(&pool, d, u, Some("u"), Some("path"), DepSource::Cargo, 1.0)
        .await
        .expect("re-upsert");
    assert_eq!(
        store::dependencies_of(&pool, d).await.expect("deps3").len(),
        1,
        "re-declared dependency is live again"
    );
}
