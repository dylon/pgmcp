//! Oracle tests for `experiment_list`.
//!
//! These pin the boundary modeled in
//! `docs/formal/tla/ExperimentListScope.tla`: validated enum filters,
//! positive project scoping, bounded pagination, deterministic newest-first
//! ordering, and read-only execution.

use crate::common::{server_with_pool, text_of};
use pgmcp_testing::pool_tool_helpers::seed_project;
use pgmcp_testing::require_test_db;
use serde_json::{Value, json};
use uuid::Uuid;

async fn insert_experiment(
    pool: &sqlx::PgPool,
    project_id: i32,
    slug: &str,
    kind: &str,
    status: &str,
    updated_offset_seconds: i32,
) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO experiments
            (slug, title, question, kind, project_id, status, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6,
                 NOW() + ($7::int * INTERVAL '1 second'))
         RETURNING id",
    )
    .bind(slug)
    .bind(format!("title {slug}"))
    .bind(format!("question {slug}"))
    .bind(kind)
    .bind(project_id)
    .bind(status)
    .bind(updated_offset_seconds)
    .fetch_one(pool)
    .await
    .expect("insert experiment")
}

async fn experiment_count(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM experiments")
        .fetch_one(pool)
        .await
        .expect("count experiments")
}

#[tokio::test]
async fn experiment_list_trims_filters_bounds_page_scopes_project_and_is_read_only() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let suffix = Uuid::new_v4().simple();
    let target_project = seed_project(
        &pool,
        &format!("elist-target-{suffix}"),
        &format!("/ws/elist-target-{suffix}"),
    )
    .await;
    let other_project = seed_project(
        &pool,
        &format!("elist-other-{suffix}"),
        &format!("/ws/elist-other-{suffix}"),
    )
    .await;
    let expected_slug = format!("elist-target-open-{suffix}");
    insert_experiment(
        &pool,
        target_project,
        &expected_slug,
        "optimization",
        "open",
        30,
    )
    .await;
    insert_experiment(
        &pool,
        target_project,
        &format!("elist-target-measuring-{suffix}"),
        "optimization",
        "measuring",
        20,
    )
    .await;
    insert_experiment(
        &pool,
        other_project,
        &format!("elist-other-open-{suffix}"),
        "optimization",
        "open",
        40,
    )
    .await;
    let before = experiment_count(&pool).await;

    let server = server_with_pool(pool.clone());
    let result = server
        .call_tool_cli(
            "experiment_list",
            json!({
                "project_id": target_project,
                "kind": " Optimization ",
                "status": " OPEN ",
                "limit": 999,
                "offset": -10,
            }),
        )
        .await
        .expect("experiment_list call");
    let v: Value = serde_json::from_str(&text_of(&result)).expect("json");
    assert_eq!(v["limit"].as_i64(), Some(500));
    assert_eq!(v["offset"].as_i64(), Some(0));
    assert_eq!(
        v["filters"]["project_id"].as_i64(),
        Some(target_project as i64)
    );
    assert_eq!(v["filters"]["kind"].as_str(), Some("optimization"));
    assert_eq!(v["filters"]["status"].as_str(), Some("open"));
    assert_eq!(v["count"].as_i64(), Some(1));
    assert_eq!(
        v["experiments"][0]["slug"].as_str(),
        Some(expected_slug.as_str())
    );
    assert_eq!(
        experiment_count(&pool).await,
        before,
        "list must not write experiment rows"
    );
}

#[tokio::test]
async fn experiment_list_rejects_invalid_filters_before_querying() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let server = server_with_pool(pool);

    assert!(
        server
            .call_tool_cli("experiment_list", json!({ "kind": "performance" }))
            .await
            .is_err(),
        "unknown kind must reject"
    );
    assert!(
        server
            .call_tool_cli("experiment_list", json!({ "status": "done" }))
            .await
            .is_err(),
        "unknown status must reject"
    );
    assert!(
        server
            .call_tool_cli("experiment_list", json!({ "project_id": 0 }))
            .await
            .is_err(),
        "non-positive project_id must reject"
    );
}

#[tokio::test]
async fn experiment_list_orders_newest_first_and_applies_offset() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let suffix = Uuid::new_v4().simple();
    let project_id = seed_project(
        &pool,
        &format!("elist-order-{suffix}"),
        &format!("/ws/elist-order-{suffix}"),
    )
    .await;
    let old_slug = format!("elist-old-{suffix}");
    let mid_slug = format!("elist-mid-{suffix}");
    let new_slug = format!("elist-new-{suffix}");
    insert_experiment(&pool, project_id, &old_slug, "bugfix", "open", 10).await;
    insert_experiment(&pool, project_id, &mid_slug, "bugfix", "open", 20).await;
    insert_experiment(&pool, project_id, &new_slug, "bugfix", "open", 30).await;

    let server = server_with_pool(pool);
    let result = server
        .call_tool_cli(
            "experiment_list",
            json!({
                "project_id": project_id,
                "kind": "bugfix",
                "status": "open",
                "limit": 2,
                "offset": 1,
            }),
        )
        .await
        .expect("experiment_list call");
    let v: Value = serde_json::from_str(&text_of(&result)).expect("json");
    let slugs: Vec<&str> = v["experiments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["slug"].as_str().unwrap())
        .collect();
    assert_eq!(slugs, vec![mid_slug.as_str(), old_slug.as_str()]);
    assert_eq!(v["count"].as_i64(), Some(2));
}
