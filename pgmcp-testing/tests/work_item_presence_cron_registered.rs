//! Phase 8 — verifies the work-item-presence/lease-decay cron is both callable
//! and actually wired into the scheduler's `schedule_maintenance_jobs`.
//!
//! Mirrors `topic_dendrogram_cron_registered.rs`. The test pair fails if either
//! piece is removed:
//!
//! 1. `direct_run_or_log_increments_counter` calls the cron's entry point
//!    directly against an empty database; `presence_sweeps` (incremented
//!    unconditionally at the top of every sweep) must move from 0 to >= 1.
//!
//! 2. `scheduler_source_registers_work_item_presence` reads the scheduler
//!    source and asserts the registration block (the string
//!    `"work-item-presence"` inside a `schedule_recurring` invocation, plus the
//!    dispatch into `work_item_presence::run_or_log`) is present. Source-level
//!    introspection is the only cheap way to assert registration without
//!    standing up a full daemon.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::require_test_db;

#[tokio::test(flavor = "multi_thread")]
async fn direct_run_or_log_increments_counter() {
    let testdb = require_test_db!();
    let pool = testdb.pool().clone();
    let stats = Arc::new(StatsTracker::new());
    let before = stats.presence_sweeps.load(Ordering::Relaxed);
    // idle=300s, offline=900s — the daemon defaults; against an empty DB this
    // simply sweeps zero rows but still records the sweep.
    pgmcp::cron::work_item_presence::run_or_log(pool, Arc::clone(&stats), 300, 900).await;
    let after = stats.presence_sweeps.load(Ordering::Relaxed);
    assert!(
        after >= before + 1,
        "presence_sweeps must increment on each run_or_log call: {before} -> {after}"
    );
}

#[test]
fn scheduler_source_registers_work_item_presence() {
    // CARGO_MANIFEST_DIR points at pgmcp-testing; go one level up to the root.
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let path = std::path::Path::new(&manifest)
        .parent()
        .expect("workspace root")
        .join("src/cron/scheduler.rs");
    let src =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

    assert!(
        src.contains("\"work-item-presence\""),
        "scheduler.rs must contain a schedule_recurring entry named \"work-item-presence\""
    );
    assert!(
        src.contains("work_item_presence::run_or_log"),
        "scheduler.rs must dispatch into cron::work_item_presence::run_or_log from the registration block"
    );
}
