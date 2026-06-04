//! ADR-009 §4.2 — verify the temporal graph-RAG tools surface the
//! `project_depends_on` neighborhood. With a live D→U dependency edge,
//! `dependency_graph{U}` must report a cross-project *dependent* (D) and
//! `dependency_graph{D}` a cross-project *dependency* (U). This locks the
//! `cross_project_blocks` wiring added to dependency_graph / centrality_analysis
//! / effect_propagation / code_ppr_search.

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

#[tokio::test(flavor = "multi_thread")]
async fn dependency_graph_surfaces_cross_project_neighborhood() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let u = seed_project(&pool, "xpg-u", "/ws/xpg-u").await;
    let d = seed_project(&pool, "xpg-d", "/ws/xpg-d").await;

    // D depends on U (a path dependency, like a Cargo `path = "../xpg-u"`).
    upsert_dep(&pool, d, u).await;

    let server = server_with_pool(pool.clone());

    // From U's side: D is a cross-project *dependent* (D may break when U changes).
    let res = server
        .call_tool_cli(
            "dependency_graph",
            serde_json::json!({ "project": "xpg-u" }),
        )
        .await
        .expect("dependency_graph U ok");
    let ju = tool_json(&res);
    assert_eq!(ju["cross_project_dependent_count"], 1, "U has 1 dependent");
    assert_eq!(ju["cross_project_dependents"][0]["project"], "xpg-d");
    assert_eq!(
        ju["cross_project_dependency_count"], 0,
        "U depends on nothing"
    );

    // From D's side: U is a cross-project *dependency* (D depends on U).
    let res = server
        .call_tool_cli(
            "dependency_graph",
            serde_json::json!({ "project": "xpg-d" }),
        )
        .await
        .expect("dependency_graph D ok");
    let jd = tool_json(&res);
    assert_eq!(
        jd["cross_project_dependency_count"], 1,
        "D has 1 dependency"
    );
    assert_eq!(jd["cross_project_dependencies"][0]["project"], "xpg-u");
    assert_eq!(
        jd["cross_project_dependent_count"], 0,
        "nothing depends on D"
    );

    // centrality_analysis carries the same neighborhood (empty graph is fine —
    // the cross-project block is independent of the code graph).
    let res = server
        .call_tool_cli(
            "centrality_analysis",
            serde_json::json!({ "project": "xpg-u" }),
        )
        .await
        .expect("centrality_analysis U ok");
    let jc = tool_json(&res);
    assert_eq!(jc["cross_project_dependent_count"], 1);
    assert_eq!(jc["cross_project_dependents"][0]["project"], "xpg-d");
}

#[tokio::test(flavor = "multi_thread")]
async fn proactive_warning_lists_dirty_edited_deps_until_coordinated() {
    // ADR-009 §4.6: a dependency that is dirty AND has a live editor AND is not
    // yet coordinated about is surfaced as a proactive warning; opening a
    // coordination request dedups it.
    let db = require_test_db!();
    let pool = db.pool().clone();
    let u = seed_project(&pool, "pw-u", "/ws/pw-u").await;
    let d = seed_project(&pool, "pw-d", "/ws/pw-d").await;
    upsert_dep(&pool, d, u).await;

    sqlx::query("UPDATE projects SET git_dirty = TRUE WHERE id = $1")
        .bind(u)
        .execute(&pool)
        .await
        .expect("mark U dirty");
    sqlx::query(
        "INSERT INTO mcp_clients (mcp_session_id, client_name, project_id, alive)
         VALUES ('sess-PWE', 'codex', $1, TRUE)",
    )
    .bind(u)
    .execute(&pool)
    .await
    .expect("seed live editor on U");

    let warns = pgmcp::deps::coord_store::pending_dependency_warnings(&pool, d, 5)
        .await
        .expect("warnings");
    assert_eq!(
        warns.len(),
        1,
        "D is warned about its dirty, edited dependency U"
    );
    assert_eq!(warns[0].dependency_name, "pw-u");
    assert!(
        warns[0].editors.as_deref().unwrap_or("").contains("codex"),
        "the live editor is named: {:?}",
        warns[0].editors
    );

    // Opening a coordination request dedups the warning.
    let server = server_with_pool(pool.clone());
    server
        .call_tool_cli(
            "coordinate_dependency_block",
            serde_json::json!({
                "dependency": "pw-u",
                "dependent_project": "pw-d",
                "requester_session": "sess-PWD"
            }),
        )
        .await
        .expect("coordinate ok");
    let warns2 = pgmcp::deps::coord_store::pending_dependency_warnings(&pool, d, 5)
        .await
        .expect("warnings2");
    assert!(
        warns2.is_empty(),
        "an open coordination request silences the proactive warning"
    );
}

/// Upsert a live `manual` D→U dependency edge via the store API.
async fn upsert_dep(pool: &sqlx::PgPool, dependent: i32, dependency: i32) {
    pgmcp::deps::store::upsert_dependency(
        pool,
        dependent,
        dependency,
        Some("xpg-u"),
        Some("path"),
        pgmcp::deps::DepSource::Manual,
        1.0,
    )
    .await
    .expect("upsert dependency");
}
