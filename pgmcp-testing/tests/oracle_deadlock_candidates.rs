//! Focused oracle coverage for `deadlock_candidates`.

mod common;

use common::text_of;
use pgmcp::parsing::type_tags::vocabulary::EFFECT_UNSAFE;
use pgmcp_testing::pool_tool_helpers::{
    seed_file, seed_file_symbol, seed_project, server_with_pool,
};
use pgmcp_testing::require_test_db;
use serde_json::{Value, json};
use uuid::Uuid;

async fn set_file_content(pool: &sqlx::PgPool, file_id: i64, content: &str) {
    sqlx::query("UPDATE indexed_files SET content = $1 WHERE id = $2")
        .bind(content)
        .bind(file_id)
        .execute(pool)
        .await
        .expect("set file content");
}

#[tokio::test(flavor = "multi_thread")]
async fn deadlock_candidates_scopes_edges_and_effects_to_project() {
    let db = require_test_db!();
    let pool = db.pool();
    let suffix = Uuid::new_v4().simple();
    let target_name = format!("dl-target-{suffix}");
    let other_name = format!("dl-other-{suffix}");
    let target_path = format!("/ws/{target_name}");
    let other_path = format!("/ws/{other_name}");

    let target_project = seed_project(pool, &target_name, &target_path).await;
    let other_project = seed_project(pool, &other_name, &other_path).await;
    let target_file = seed_file(pool, target_project, &format!("{target_path}/a.rs"), "a.rs").await;
    let other_file = seed_file(pool, other_project, &format!("{other_path}/b.rs"), "b.rs").await;

    set_file_content(
        pool,
        target_file,
        r#"
        fn first() {
            alpha.lock();
            beta.lock();
        }
        fn second() {
            beta.lock();
            alpha.lock();
        }
        "#,
    )
    .await;
    set_file_content(
        pool,
        other_file,
        r#"
        fn other_first() {
            gamma.lock();
            delta.lock();
        }
        fn other_second() {
            delta.lock();
            gamma.lock();
        }
        "#,
    )
    .await;

    let other_symbol =
        seed_file_symbol(pool, other_file, "other_unsafe", "function", 1, None).await;
    sqlx::query("INSERT INTO symbol_effects (symbol_id, effect) VALUES ($1, $2)")
        .bind(other_symbol)
        .bind(EFFECT_UNSAFE)
        .execute(pool)
        .await
        .expect("seed other-project effect");

    let server = server_with_pool(pool.clone());
    let result = server
        .call_tool_cli("deadlock_candidates", json!({ "project": target_name }))
        .await
        .expect("deadlock_candidates");
    let body: Value = serde_json::from_str(&text_of(&result)).expect("deadlock json");

    let edges = body["edges"].as_array().expect("edges array");
    assert!(
        edges
            .iter()
            .any(|edge| edge["from"] == "alpha" && edge["to"] == "beta"),
        "target project alpha -> beta edge is present"
    );
    assert!(
        edges
            .iter()
            .any(|edge| edge["from"] == "beta" && edge["to"] == "alpha"),
        "target project beta -> alpha edge is present"
    );
    assert!(
        edges
            .iter()
            .all(|edge| edge["from"] != "gamma" && edge["to"] != "delta"),
        "other-project lock edges must not leak"
    );

    let cycles = body["cycles"].as_array().expect("cycles array");
    assert!(
        cycles.iter().any(|cycle| {
            let locks = cycle.as_array().expect("cycle locks");
            locks.iter().any(|lock| lock == "alpha") && locks.iter().any(|lock| lock == "beta")
        }),
        "target project alpha/beta cycle is present"
    );

    let effects = body["effect_breakdown"]
        .as_object()
        .expect("effect breakdown object");
    assert!(
        effects.is_empty(),
        "effect_breakdown must not include effects from another project: {effects:?}"
    );
}
