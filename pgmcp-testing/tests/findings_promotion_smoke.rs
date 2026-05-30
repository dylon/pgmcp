//! Integration test for the `findings-promotion` cron's idempotency (Phase 3).
//!
//! Seeds an opted-in project (a TempDir with `[tracker] auto_promote_findings =
//! true` in its `.pgmcp.toml`) carrying one indexed file with a high-severity
//! `// FIXME` marker, runs `findings_promotion::run_or_log` TWICE, and asserts
//! exactly ONE `pending` `fixme` work item was created (re-running the cron must
//! not duplicate). Also asserts the lower-level `promote_finding` idempotency
//! guarantee directly. Self-skips via `require_test_db!`.

use std::sync::Arc;

use pgmcp::db::queries::{self, FindingAnchor, NewWorkItem};
use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::require_test_db;
use sqlx::PgPool;

/// Seed a project rooted at `repo` with one file containing a FIXME marker.
async fn seed_project_with_fixme(pool: &PgPool, repo: &str) -> i32 {
    let project_id: i32 = sqlx::query_scalar(
        "INSERT INTO projects (workspace_path, path, name) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(repo)
    .bind(repo)
    .bind("findings-promo-test")
    .fetch_one(pool)
    .await
    .expect("seed project");

    sqlx::query(
        "INSERT INTO indexed_files \
            (project_id, path, relative_path, language, size_bytes, content, line_count, modified_at) \
         VALUES ($1, $2, $3, 'rust', 64, $4, 3, NOW())",
    )
    .bind(project_id)
    .bind(format!("{repo}/src/buggy.rs"))
    .bind("src/buggy.rs")
    .bind("fn buggy() {\n    // FIXME: this leaks on the error path\n}\n")
    .execute(pool)
    .await
    .expect("seed file");

    project_id
}

#[tokio::test]
async fn findings_promotion_runs_twice_yields_one_item() {
    let db = require_test_db!();
    let pool = db.pool().clone();

    // A TempDir project root with the opt-in .pgmcp.toml the cron reads.
    let workdir = tempfile::TempDir::new().expect("tempdir");
    let repo = workdir.path().to_str().expect("utf8 path").to_string();
    std::fs::write(
        workdir.path().join(".pgmcp.toml"),
        "[tracker]\nauto_promote_findings = true\n",
    )
    .expect("write .pgmcp.toml");

    let project_id = seed_project_with_fixme(&pool, &repo).await;

    let stats = Arc::new(StatsTracker::new());

    // ── run the cron TWICE ──
    pgmcp::cron::findings_promotion::run_or_log(pool.clone(), Arc::clone(&stats)).await;
    pgmcp::cron::findings_promotion::run_or_log(pool.clone(), Arc::clone(&stats)).await;

    // Exactly one fixme item, in pending, for this project.
    let items: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT public_id, kind, status FROM work_items WHERE project_id = $1 AND kind = 'fixme'",
    )
    .bind(project_id)
    .fetch_all(&pool)
    .await
    .expect("query items");
    assert_eq!(
        items.len(),
        1,
        "running the cron twice must yield exactly one fixme item, got {items:?}"
    );
    assert_eq!(items[0].2, "pending", "promoted findings land in pending");

    // Exactly one provenance row for that item (the idempotency ledger).
    let prov_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM work_item_finding_provenance \
         WHERE finding_source = 'documented_tech_debt'",
    )
    .fetch_one(&pool)
    .await
    .expect("count provenance");
    assert_eq!(prov_count, 1, "one provenance row per distinct finding");

    // The created counter advanced on the FIRST run only (the second was a
    // no-op), so exactly 1 promotion is recorded.
    assert_eq!(
        stats
            .findings_promoted
            .load(std::sync::atomic::Ordering::Relaxed),
        1,
        "the second run promotes nothing new"
    );
}

/// The lower-level guarantee the cron rides on: `promote_finding` keyed by a
/// `provenance_key` inserts once, then returns the same item with
/// `created=false` — even across many calls.
#[tokio::test]
async fn promote_finding_is_idempotent_on_provenance_key() {
    let db = require_test_db!();
    let pool = db.pool().clone();

    let key = "bug_prediction:findings-promo-test:src/hot.rs";
    let mk_item = || NewWorkItem {
        public_id: "", // filled per call below
        kind: "bug",
        status: "pending",
        title: "Bug-prone file: src/hot.rs",
        ..Default::default()
    };

    // First promotion → created.
    let pid1 = format!(
        "finding-bug-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    let mut item1 = mk_item();
    item1.public_id = &pid1;
    let (id1, created1) = queries::promote_finding(
        &pool,
        key,
        "bug_prediction",
        item1,
        FindingAnchor::default(),
    )
    .await
    .expect("first promote");
    assert!(created1, "first promotion creates the item");

    // Second + third promotions of the SAME key → existing id, created=false.
    for _ in 0..2 {
        let pid = format!(
            "finding-bug-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        );
        let mut item = mk_item();
        item.public_id = &pid;
        let (id, created) =
            queries::promote_finding(&pool, key, "bug_prediction", item, FindingAnchor::default())
                .await
                .expect("repeat promote");
        assert_eq!(id, id1, "the same provenance key returns the same item id");
        assert!(!created, "a repeat promotion does not create a new item");
    }

    // Only one work item exists for that key's provenance row.
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM work_items w \
         JOIN work_item_finding_provenance p ON p.item_id = w.id \
         WHERE p.provenance_key = $1",
    )
    .bind(key)
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(
        count, 1,
        "exactly one item materialized for the provenance key"
    );
}
