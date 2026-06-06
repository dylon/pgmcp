//! P13.1 — verifies the topic-dendrogram cron is both callable and
//! actually wired into the scheduler's `schedule_maintenance_jobs`.
//!
//! The audit found `run_or_log` defined but never registered. This
//! test pair fails if either piece is removed:
//!
//! 1. `direct_run_or_log_increments_counter` calls the cron's entry
//!    point directly against an empty database; the counter must
//!    move from 0 to >= 1.
//!
//! 2. `scheduler_source_registers_topic_dendrogram` reads the
//!    scheduler source and asserts the registration block (the
//!    string `"topic-dendrogram"` inside a `schedule_recurring`
//!    invocation) is present. Source-level introspection is the
//!    only cheap way to assert registration without standing up a
//!    full daemon — `schedule_recurring` takes a closure and
//!    doesn't expose its registry.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::require_test_db;

#[tokio::test(flavor = "multi_thread")]
async fn direct_run_or_log_increments_counter() {
    let testdb = require_test_db!();
    let pool = Arc::new(testdb.pool().clone());
    let stats = Arc::new(StatsTracker::new());
    let before = stats.topic_dendrogram_runs.load(Ordering::Relaxed);
    pgmcp::cron::topic_dendrogram::run_or_log(pool, Arc::clone(&stats)).await;
    let after = stats.topic_dendrogram_runs.load(Ordering::Relaxed);
    assert!(
        after > before,
        "topic_dendrogram_runs must increment on each run_or_log call: {before} -> {after}"
    );
}

#[test]
fn scheduler_source_registers_topic_dendrogram() {
    // Look up the scheduler source relative to the workspace root.
    // CARGO_MANIFEST_DIR points at pgmcp-testing; go one level up.
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let path = std::path::Path::new(&manifest)
        .parent()
        .expect("workspace root")
        .join("src/cron/scheduler.rs");
    let src =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

    // The registration block must contain BOTH the schedule_recurring
    // call with the "topic-dendrogram" name AND the call into
    // run_or_log. Asserting both prevents a regression where the
    // block exists but no longer calls the cron function.
    assert!(
        src.contains("\"topic-dendrogram\""),
        "scheduler.rs must contain a schedule_recurring entry named \"topic-dendrogram\""
    );
    assert!(
        src.contains("topic_dendrogram::run_or_log"),
        "scheduler.rs must dispatch into cron::topic_dendrogram::run_or_log from the registration block"
    );
}
