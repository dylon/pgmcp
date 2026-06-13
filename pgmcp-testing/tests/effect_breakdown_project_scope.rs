//! Regression: the `effect_breakdown` channel is PROJECT-scoped, not
//! workspace-wide.
//!
//! Before the fix, every tool emitting `effect_breakdown` ran a workspace-wide
//! `GROUP BY se.effect` with no project join, so a tool called with project A
//! returned project B's effect counts (and bloated every response with the same
//! blob). The fix routes all scoped tools through
//! [`effect_breakdown_json`](pgmcp::mcp::tools::sema_helpers::effects::effect_breakdown_json)
//! / [`effect_counts`](pgmcp::mcp::tools::sema_helpers::effects::effect_counts).
//! This seeds two projects with disjoint effects and pins that the helpers
//! return each project's own distribution — never the other's, never the union.

use std::collections::HashMap;

use pgmcp::mcp::tools::sema_helpers::effects::{effect_breakdown_json, effect_counts};
use pgmcp_testing::pool_tool_helpers::{seed_file, seed_file_symbol, seed_project};
use pgmcp_testing::require_test_db;
use uuid::Uuid;

async fn seed_symbol_with_effects(
    pool: &sqlx::PgPool,
    project_id: i32,
    tag: &str,
    effects: &[&str],
) {
    let file_id = seed_file(
        pool,
        project_id,
        &format!("/ws/{tag}/lib.rs"),
        &format!("{tag}/lib.rs"),
    )
    .await;
    let symbol_id = seed_file_symbol(pool, file_id, &format!("f_{tag}"), "function", 1, None).await;
    for eff in effects {
        sqlx::query("INSERT INTO symbol_effects (symbol_id, effect) VALUES ($1, $2)")
            .bind(symbol_id)
            .bind(*eff)
            .execute(pool)
            .await
            .expect("seed symbol_effect");
    }
}

#[tokio::test]
async fn effect_breakdown_is_project_scoped_not_workspace_wide() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let suffix = Uuid::new_v4().simple();

    let proj_a = seed_project(
        &pool,
        &format!("eb-a-{suffix}"),
        &format!("/ws/eb-a-{suffix}"),
    )
    .await;
    let proj_b = seed_project(
        &pool,
        &format!("eb-b-{suffix}"),
        &format!("/ws/eb-b-{suffix}"),
    )
    .await;

    // Disjoint effect distributions per project.
    seed_symbol_with_effects(&pool, proj_a, &format!("a{suffix}"), &["async", "pure"]).await;
    seed_symbol_with_effects(&pool, proj_b, &format!("b{suffix}"), &["may_panic"]).await;

    let counts_a: HashMap<String, i64> = effect_counts(&pool, proj_a).await.expect("counts a");
    let counts_b: HashMap<String, i64> = effect_counts(&pool, proj_b).await.expect("counts b");

    // Each project sees ONLY its own effects — the project filter is the whole point.
    assert_eq!(counts_a.get("async"), Some(&1), "A has its async");
    assert_eq!(counts_a.get("pure"), Some(&1), "A has its pure");
    assert_eq!(
        counts_a.get("may_panic"),
        None,
        "A must NOT see B's may_panic"
    );
    assert_eq!(counts_b.get("may_panic"), Some(&1), "B has its may_panic");
    assert_eq!(counts_b.get("async"), None, "B must NOT see A's async");
    assert_ne!(
        counts_a, counts_b,
        "the two projects must have different effect distributions"
    );

    // The rendered JSON breakdown is likewise project-scoped and differs by project.
    let json_a = effect_breakdown_json(&pool, Some(proj_a)).await;
    let json_b = effect_breakdown_json(&pool, Some(proj_b)).await;
    assert_ne!(
        json_a, json_b,
        "effect_breakdown_json must differ by project"
    );
    // The breakdown is now a `{effect: count}` object map; effects are its keys.
    let a_effects: Vec<&str> = json_a
        .as_object()
        .map(|o| o.keys().map(String::as_str).collect())
        .unwrap_or_default();
    assert!(
        a_effects.contains(&"async") && a_effects.contains(&"pure"),
        "A's breakdown must contain its own effects: {a_effects:?}"
    );
    assert!(
        !a_effects.contains(&"may_panic"),
        "A's breakdown must NOT contain B's effect: {a_effects:?}"
    );

    // None → empty object: the honest "no project scope" signal for project-less
    // tools, never the old workspace-wide blob.
    assert!(
        effect_breakdown_json(&pool, None)
            .await
            .as_object()
            .is_some_and(|o| o.is_empty()),
        "a None project_id yields an empty breakdown"
    );
}
