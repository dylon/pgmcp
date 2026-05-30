//! Phase 3 (git/PR close-the-loop) — verifies the `findings-promotion` cron is
//! wired into the scheduler and declared as a module.
//!
//! Mirrors the source-introspection half of `quality_history_cron_registered.rs`.
//! `findings_promotion::run_or_log` takes a bare pool (a light job), so a
//! DB-backed run-twice idempotency test lives in `findings_promotion_smoke.rs`;
//! this file is the always-run guard that fails the moment either the scheduler
//! registration or the module declaration is removed:
//!
//! 1. `scheduler_source_registers_findings_promotion` reads
//!    `src/cron/scheduler.rs` and asserts both the `"findings-promotion"`
//!    schedule-name literal and the dispatch into
//!    `findings_promotion::run_or_log` are present.
//!
//! 2. `cron_mod_declares_findings_promotion` reads `src/cron/mod.rs` and asserts
//!    the `pub mod findings_promotion;` declaration is present.

use std::path::{Path, PathBuf};

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
fn scheduler_source_registers_findings_promotion() {
    let src = read("src/cron/scheduler.rs");
    assert!(
        src.contains("\"findings-promotion\""),
        "scheduler.rs must contain a schedule_recurring entry named \"findings-promotion\""
    );
    assert!(
        src.contains("findings_promotion::run_or_log"),
        "scheduler.rs must dispatch into cron::findings_promotion::run_or_log from the registration block"
    );
}

#[test]
fn cron_mod_declares_findings_promotion() {
    let src = read("src/cron/mod.rs");
    assert!(
        src.contains("pub mod findings_promotion;"),
        "src/cron/mod.rs must declare `pub mod findings_promotion;`"
    );
}
