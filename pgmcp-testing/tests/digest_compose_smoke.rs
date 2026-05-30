//! Phase 4 — `compose_digest` end-to-end smoke.
//!
//! Seeds a project plus three tracker rows that land in three different digest
//! buckets — an overdue task, a blocked task, and a bug awaiting triage — and
//! asserts that:
//!
//! 1. `compose_digest` surfaces all three (the TRACKER section names them), and
//! 2. `render_markdown` honours the byte budget (a tight budget truncates,
//!    keeping the most-severe item; the rendered block never exceeds the cap).
//!
//! Self-skips (via `require_test_db!`) when no test DB is configured.

use chrono::{Duration, Utc};
use pgmcp::config::DigestConfig;
use pgmcp::db::queries::{NewWorkItem, insert_work_item};
use pgmcp::digest::compose_digest;
use pgmcp_testing::require_test_db;
use sqlx::PgPool;

/// Insert a project, returning its id.
async fn seed_project(pool: &PgPool, name: &str) -> i32 {
    sqlx::query_scalar::<_, i32>(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind("/ws")
    .bind(format!("/ws/{name}/"))
    .bind(name)
    .fetch_one(pool)
    .await
    .expect("seed project")
}

/// Insert a minimal work item in the given initial status, returning its id.
async fn seed_item(
    pool: &PgPool,
    project_id: i32,
    public_id: &str,
    kind: &str,
    status: &str,
    title: &str,
) -> i64 {
    insert_work_item(
        pool,
        NewWorkItem {
            public_id,
            parent_id: None,
            project_id: Some(project_id),
            definition_id: None,
            kind,
            status,
            title,
            body: None,
            priority: 50,
            weight: 1.0,
            parametric: false,
            parametric_corpus: None,
            parametric_expected: None,
            origin: "test",
            created_by: Some("smoke"),
            severity: if kind == "bug" { Some("high") } else { None },
            embedding: None,
        },
    )
    .await
    .expect("insert work item")
}

#[tokio::test(flavor = "multi_thread")]
async fn compose_surfaces_overdue_blocked_triage_and_budgets() {
    let db = require_test_db!();
    let pool = db.pool().clone();
    let project_id = seed_project(&pool, "digest-smoke-proj").await;

    // Overdue: a task with a past due_at, still open.
    let overdue_id = seed_item(
        &pool,
        project_id,
        "dg-overdue",
        "task",
        "pending",
        "overdue task",
    )
    .await;
    let past = Utc::now() - Duration::days(3);
    sqlx::query("UPDATE work_items SET due_at = $1 WHERE id = $2")
        .bind(past)
        .bind(overdue_id)
        .execute(&pool)
        .await
        .expect("set past due_at");

    // Blocked: a task in status='blocked'.
    seed_item(
        &pool,
        project_id,
        "dg-blocked",
        "task",
        "blocked",
        "blocked task",
    )
    .await;

    // Needs-triage: a bug born in status='triage'.
    seed_item(
        &pool,
        project_id,
        "dg-triage",
        "bug",
        "triage",
        "triage bug",
    )
    .await;

    let cfg = DigestConfig {
        enabled: true,
        ..DigestConfig::default()
    };
    let digest = compose_digest(&pool, Some(project_id), None, &cfg).await;

    assert!(!digest.is_empty(), "digest should carry tracker items");

    // All three buckets name their public_ids in the item text.
    let joined: String = digest
        .items
        .iter()
        .map(|i| i.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("dg-overdue"),
        "overdue item named:\n{joined}"
    );
    assert!(
        joined.contains("dg-blocked"),
        "blocked item named:\n{joined}"
    );
    assert!(joined.contains("dg-triage"), "triage item named:\n{joined}");

    // Bucket labels present.
    assert!(joined.contains("overdue"), "overdue label:\n{joined}");
    assert!(joined.contains("blocked"), "blocked label:\n{joined}");
    assert!(joined.contains("triage"), "triage label:\n{joined}");

    // The overdue bucket is High severity (most urgent tracker signal).
    assert_eq!(
        digest.max_severity(),
        Some(pgmcp::digest::DigestSeverity::High),
        "overdue ⇒ High is the headline severity"
    );

    // Full render contains the section + the overdue (highest-severity) line.
    let full = digest.render_markdown(4096);
    assert!(
        full.starts_with("## pgmcp digest"),
        "render header:\n{full}"
    );
    assert!(
        full.contains("dg-overdue"),
        "full render names overdue:\n{full}"
    );

    // Byte budget: a tight cap truncates, never exceeding the cap, and keeps the
    // most-severe (overdue, High) item.
    let budget = 120usize;
    let tight = digest.render_markdown(budget);
    assert!(
        tight.len() <= budget,
        "render exceeded the {budget}-byte budget: {} bytes\n{tight}",
        tight.len()
    );
    assert!(
        tight.contains("overdue"),
        "tight render must keep the most-severe (overdue) bucket:\n{tight}"
    );
    assert!(
        full.len() >= tight.len(),
        "a larger budget renders at least as much"
    );
}
