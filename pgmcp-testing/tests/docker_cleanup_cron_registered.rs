//! Verifies the `docker-cleanup` cron is wired into the scheduler and declared
//! as a module, and that its public entry point is a safe no-op when docker is
//! unavailable. Mirrors the source-introspection half of
//! `topics_size_history_cron_registered.rs`. The pure parsing logic
//! (`parse_human_size` / `parse_reclaimed` / `parse_system_df_reclaimable`) and
//! the unavailable-docker behavior are unit-tested in `src/cron/docker_cleanup.rs`.

use std::path::{Path, PathBuf};

use pgmcp::config::DockerCleanupConfig;

fn repo_root() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    Path::new(&manifest)
        .parent()
        .expect("workspace root above pgmcp-testing")
        .to_path_buf()
}

#[test]
fn scheduler_source_registers_docker_cleanup() {
    let src = std::fs::read_to_string(repo_root().join("src/cron/scheduler.rs"))
        .expect("read scheduler.rs");
    assert!(
        src.contains("\"docker-cleanup\""),
        "scheduler must register the docker-cleanup schedule name"
    );
    assert!(
        src.contains("docker_cleanup::run_or_log"),
        "scheduler must dispatch into docker_cleanup::run_or_log"
    );
}

#[test]
fn cron_mod_declares_docker_cleanup() {
    let src =
        std::fs::read_to_string(repo_root().join("src/cron/mod.rs")).expect("read cron/mod.rs");
    assert!(
        src.contains("pub mod docker_cleanup;"),
        "cron/mod.rs must declare the docker_cleanup module"
    );
}

/// The public async entry point completes without panicking when docker is
/// absent (a quiet no-op), exercising the `spawn_blocking` path end-to-end.
#[tokio::test]
async fn run_or_log_is_safe_when_docker_absent() {
    let cfg = DockerCleanupConfig {
        docker_bin: "pgmcp-no-such-docker-binary-xyzzy".to_string(),
        dry_run: true,
        ..Default::default()
    };
    pgmcp::cron::docker_cleanup::run_or_log(cfg).await;
}
