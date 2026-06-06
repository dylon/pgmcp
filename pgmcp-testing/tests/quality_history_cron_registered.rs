//! Phase 1 (trends & forecasting) — verifies the `quality-history` cron is
//! wired into the scheduler and declared as a module.
//!
//! Mirrors the source-introspection half of `work_item_presence_cron_registered.rs`.
//! `quality_history::run_or_log` takes a fully-built `SystemContext` (it fans out
//! the quality collectors via `quality::aggregate`), not a bare pool, so unlike
//! the presence cron there is no cheap empty-DB direct-call half — standing up a
//! production `SystemContext` purely to count a sweep would be heavier than the
//! signal is worth. Source-level introspection is the always-run guard that
//! fails the moment either the scheduler registration or the module declaration
//! is removed:
//!
//! 1. `scheduler_source_registers_quality_history` reads `src/cron/scheduler.rs`
//!    and asserts both the `"quality-history"` schedule-name literal and the
//!    dispatch into `quality_history::run_or_log` are present.
//!
//! 2. `cron_mod_declares_quality_history` reads `src/cron/mod.rs` and asserts the
//!    `pub mod quality_history;` declaration is present.
//!
//! 3. `quality_history_snapshots_duplicate_project_names_by_id` verifies the
//!    runtime cron uses the concrete ids returned by `list_projects` instead of
//!    re-resolving ambiguous display names.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::pool_tool_helpers::{context_with_pool, seed_file, seed_project};
use pgmcp_testing::require_test_db;

/// Repo root (one level above pgmcp-testing's `CARGO_MANIFEST_DIR`).
fn repo_root() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    Path::new(&manifest)
        .parent()
        .expect("workspace root above pgmcp-testing")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn scheduler_source_registers_quality_history() {
    let src = read("src/cron/scheduler.rs");
    assert!(
        src.contains("\"quality-history\""),
        "scheduler.rs must contain a schedule_recurring entry named \"quality-history\""
    );
    assert!(
        src.contains("quality_history::run_or_log"),
        "scheduler.rs must dispatch into cron::quality_history::run_or_log from the registration block"
    );
}

#[test]
fn cron_mod_declares_quality_history() {
    let src = read("src/cron/mod.rs");
    assert!(
        src.contains("pub mod quality_history;"),
        "src/cron/mod.rs must declare `pub mod quality_history;`"
    );
}

#[tokio::test]
async fn quality_history_snapshots_duplicate_project_names_by_id() {
    let db = require_test_db!();
    let pool = db.pool().clone();

    let p1 = seed_project(&pool, "dup-quality-history", "/ws/qh-a").await;
    let p2 = seed_project(&pool, "dup-quality-history", "/ws/qh-b").await;
    seed_file(&pool, p1, "/ws/qh-a/a.rs", "a.rs").await;
    seed_file(&pool, p2, "/ws/qh-b/b.rs", "b.rs").await;

    sqlx::query("DELETE FROM quality_report_history WHERE project_id = $1 OR project_id = $2")
        .bind(p1)
        .bind(p2)
        .execute(&pool)
        .await
        .expect("clear existing history");

    let ctx = context_with_pool(pool.clone());
    let stats = Arc::new(StatsTracker::new());
    pgmcp::cron::quality_history::run_or_log(ctx, Arc::clone(&stats)).await;

    let rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM quality_report_history WHERE project_id = $1 OR project_id = $2",
    )
    .bind(p1)
    .bind(p2)
    .fetch_one(&pool)
    .await
    .expect("history rows");

    assert_eq!(
        rows, 2,
        "each duplicate-named project gets its own snapshot"
    );
    assert_eq!(
        stats.quality_history_snapshots.load(Ordering::Relaxed),
        2,
        "snapshot counter tracks the two concrete project ids"
    );
}
