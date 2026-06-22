//! Real-DB oracle for the `target-cleanup` cron.
//!
//! Exercises the full async path against a fresh, migrated `pgmcp_test_<uuid>`
//! database (so it can run in **apply** mode with no risk to real projects —
//! `list_projects` returns only the seeded fixture): discovery from the
//! `projects` table, live-git staleness, Tier-2 full wipe of a stale target,
//! source-survival (recoverability), and the provenance-first tmp sweep driven
//! by real `client_file_events` + `sessions` rows. Skips cleanly when no test DB
//! is configured (`require_test_db!`) or `git` is unavailable.
//!
//! The pure discovery / tier / guard / classification logic is unit-tested in
//! `src/cron/target_cleanup.rs`; this oracle validates the SQL and the
//! `spawn_blocking` orchestration end-to-end against the live schema.

use std::path::Path;
use std::process::Command;

use chrono::{Duration, Utc};
use pgmcp::config::TargetCleanupConfig;
use pgmcp::cron::target_cleanup::run_target_cleanup;
use pgmcp_testing::pool_tool_helpers::seed_project;
use pgmcp_testing::require_test_db;

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir -p");
    }
    std::fs::write(path, contents).expect("write file");
}

fn git(dir: &Path, env: &[(&str, &str)], args: &[&str]) -> bool {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir);
    for (k, v) in env {
        c.env(k, v);
    }
    c.args(args);
    c.output().map(|o| o.status.success()).unwrap_or(false)
}

#[tokio::test]
async fn target_cleanup_wipes_stale_target_keeps_source_and_honors_tmp_provenance() {
    let db = require_test_db!();
    let pool = db.pool().clone();

    let id = std::process::id();
    let base = std::env::temp_dir().join(format!("pgmcp_tc_oracle_{id}"));
    let _ = std::fs::remove_dir_all(&base);
    let ws = base.join("ws");
    let proj = ws.join("stale-proj");
    let tmp = base.join("tmproot");
    std::fs::create_dir_all(&tmp).expect("mkdir tmp");
    // Canonicalize so seeded provenance paths match the walker's canonical paths.
    let tmp = std::fs::canonicalize(&tmp).expect("canonicalize tmp");

    // --- a cargo project with a populated target/ ---
    write(
        &proj.join("Cargo.toml"),
        "[package]\nname=\"x\"\nversion=\"0.0.0\"\nedition=\"2021\"\n",
    );
    write(&proj.join("src/lib.rs"), "pub fn f() {}\n");
    write(
        &proj.join("target/debug/incremental/seg/part.bin"),
        "scratch",
    );
    write(&proj.join("target/debug/deps/libx.rlib"), "artifact");
    write(&proj.join("target/release/x"), "binary");

    // Git repo with one commit dated long ago (committer date drives staleness).
    if !git(&proj, &[], &["-c", "init.defaultBranch=main", "init"]) {
        eprintln!("SKIPPED: git unavailable");
        let _ = std::fs::remove_dir_all(&base);
        return;
    }
    assert!(git(&proj, &[], &["config", "user.name", "T"]));
    assert!(git(&proj, &[], &["config", "user.email", "t@t"]));
    assert!(git(&proj, &[], &["add", "-A"]));
    let old_date = "2025-01-01T00:00:00";
    assert!(
        git(
            &proj,
            &[("GIT_COMMITTER_DATE", old_date)],
            &["commit", "-m", "old", "--date", old_date, "--no-gpg-sign"],
        ),
        "git commit (stale-dated)"
    );

    // Discovery comes from the projects table (the pgmcp synergy).
    seed_project(&pool, "stale-proj", proj.to_str().expect("utf8 proj")).await;

    // --- tmp fixtures ---
    // (1) attributed to a GONE session (no live sessions row), event 200d old.
    let gone_file = tmp.join("scratch-gone.py");
    write(&gone_file, "print('gone')\n");
    let gone_uuid = uuid::Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_00a1).to_string();
    sqlx::query(
        "INSERT INTO client_file_events (session_id, abs_path, op, source, ts)
         VALUES ($1::uuid, $2, 'write', 'client_hook', $3)",
    )
    .bind(&gone_uuid)
    .bind(gone_file.to_str().expect("utf8 gone"))
    .bind(Utc::now() - Duration::days(200))
    .execute(&pool)
    .await
    .expect("insert gone event");

    // (2) attributed to a LIVE session (sessions.last_seen = now) → protected.
    let live_file = tmp.join("scratch-live.py");
    write(&live_file, "print('live')\n");
    let live_uuid = uuid::Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_00b2).to_string();
    sqlx::query("INSERT INTO sessions (id, cwd, last_seen) VALUES ($1::uuid, $2, NOW())")
        .bind(&live_uuid)
        .bind("/x")
        .execute(&pool)
        .await
        .expect("insert live session");
    sqlx::query(
        "INSERT INTO client_file_events (session_id, abs_path, op, source, ts)
         VALUES ($1::uuid, $2, 'write', 'client_hook', NOW())",
    )
    .bind(&live_uuid)
    .bind(live_file.to_str().expect("utf8 live"))
    .execute(&pool)
    .await
    .expect("insert live event");

    // (3) unattributed Bash-tool junk, aged into the past via `touch` (GNU).
    let age_file = tmp.join("old-junk.tmp");
    write(&age_file, "massif junk\n");
    let aged = Command::new("touch")
        .args(["-a", "-m", "-d", "400 days ago"])
        .arg(&age_file)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    // --- run the cron in apply mode, scoped to the throwaway tmp dir ---
    let cfg = TargetCleanupConfig {
        interval_secs: 604_800,
        dry_run: false,
        active_days: 14,
        stale_days: 60,
        build_quiet_mins: 0, // do not skip the freshly-created fixture target
        free_floor_gb: 0,
        roots: Vec::new(), // discover via the seeded project row
        allowlist: Vec::new(),
        sweep_tmp: true,
        tmp_dirs: vec![tmp.to_string_lossy().into_owned()],
        tmp_attributed_grace_secs: 3600,
        tmp_session_grace_secs: 7200,
        tmp_unattributed_age_days: 10,
        tmp_unattributed_var_age_days: 30,
        manifest_keep: 50,
    };

    let report = run_target_cleanup(&pool, &cfg, None).await;

    // --- target/ assertions: stale → full wipe; source survives (recoverable) ---
    assert!(
        !proj.join("target").exists(),
        "stale target/ wiped (Tier 2)"
    );
    assert!(proj.join("Cargo.toml").exists(), "Cargo.toml kept");
    assert!(
        proj.join("src/lib.rs").exists(),
        "source kept — removal is recoverable by rebuild"
    );
    assert!(report.tier2_bytes > 0, "reported reclaimed target bytes");
    assert_eq!(report.targets_scanned, 1, "exactly the seeded project");

    // --- tmp assertions: provenance protect/remove + age fallback ---
    assert!(
        !gone_file.exists(),
        "tmp file of a GONE agent removed (provenance)"
    );
    assert!(
        live_file.exists(),
        "tmp file of a LIVE agent protected — the guarantee age alone can't make"
    );
    assert_eq!(
        report.tmp_protected_live, 1,
        "exactly one live-protected file"
    );
    if aged {
        assert!(
            !age_file.exists(),
            "old unattributed tmp file removed (age fallback)"
        );
        assert!(report.tmp_files_removed >= 2, "gone + aged removed");
    } else {
        eprintln!("NOTE: `touch -d` unavailable; skipped the age-fallback assertion");
        assert!(report.tmp_files_removed >= 1, "gone removed");
    }

    let _ = std::fs::remove_dir_all(&base);
}
