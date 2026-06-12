//! Deterministic real-DB oracle for `work_summary`.
//!
//! Seeds one project row pointing at a throwaway git repo with three known
//! commits (May 2026, conventional subjects, pinned line churn) and asserts the
//! tool reports the right commit count, churn, type mix, active days, and date
//! span end-to-end through `call_tool_cli`. Skips cleanly when no test DB is
//! configured (`require_test_db!`) or when `git` is unavailable.
//!
//! This is the CI-runnable correctness oracle. The full-workspace "reproduce the
//! May-2026 figures (1,402 commits / 19 projects)" check is a live verification
//! against the populated daemon index, recorded in
//! `docs/formal/work-summary-traceability.md`.

use std::process::Command;

use pgmcp_testing::pool_tool_helpers::{seed_project, server_with_pool};
use pgmcp_testing::require_test_db;

/// Run a git command in `dir` with optional env, asserting success.
fn git(dir: &str, env: &[(&str, &str)], args: &[&str]) -> bool {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir);
    for (k, v) in env {
        c.env(k, v);
    }
    c.args(args);
    c.output().map(|o| o.status.success()).unwrap_or(false)
}

/// Add a new file with `lines` newline-terminated lines (= `lines` insertions in
/// `git log --numstat`) and commit it at `date` (both author + committer date).
fn add_commit(dir: &str, file: &str, lines: usize, msg: &str, date: &str) {
    let body: String = (0..lines).map(|i| format!("line {i}\n")).collect();
    std::fs::write(std::path::Path::new(dir).join(file), body).expect("write file");
    assert!(git(dir, &[], &["add", "-A"]), "git add");
    assert!(
        git(
            dir,
            &[("GIT_COMMITTER_DATE", date)],
            &["commit", "-m", msg, "--date", date, "--no-gpg-sign"],
        ),
        "git commit"
    );
}

fn text_of(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .next()
        .expect("text content present")
}

#[tokio::test]
async fn work_summary_counts_commits_churn_and_types_from_a_real_repo() {
    let db = require_test_db!();
    let pool = db.pool().clone();

    // Throwaway workspace containing one git repo `demo-proj`.
    let ws = std::env::temp_dir().join(format!("pgmcp_wlog_ws_{}", std::process::id()));
    let repo = ws.join("demo-proj");
    let _ = std::fs::remove_dir_all(&ws);
    std::fs::create_dir_all(&repo).expect("mkdir repo");
    let repo_s = repo.to_str().expect("utf8 repo path");
    let ws_s = ws.to_str().expect("utf8 ws path");

    if !git(repo_s, &[], &["-c", "init.defaultBranch=main", "init"]) {
        eprintln!("SKIPPED: git unavailable");
        let _ = std::fs::remove_dir_all(&ws);
        return;
    }
    assert!(git(repo_s, &[], &["config", "user.name", "Test Author"]));
    assert!(git(repo_s, &[], &["config", "user.email", "t@t"]));

    // Three commits: feat (+5), fix (+3), docs (+2) → 3 commits, +10/−0, 3 days.
    add_commit(
        repo_s,
        "a.txt",
        5,
        "feat(core): alpha",
        "2026-05-05T12:00:00",
    );
    add_commit(repo_s, "b.txt", 3, "fix(core): beta", "2026-05-10T12:00:00");
    add_commit(repo_s, "c.txt", 2, "docs: gamma", "2026-05-20T12:00:00");

    let project_id = seed_project(&pool, "demo-proj", repo_s).await;
    assert!(project_id > 0);

    let server = server_with_pool(pool);
    let result = server
        .call_tool_cli(
            "work_summary",
            serde_json::json!({
                "workspace_root": ws_s,
                "since": "2026-05-01",
                "until": "2026-06-01",
                "author": "Test Author",
                "format": "json",
                "use_graph": "off"
            }),
        )
        .await
        .expect("work_summary call");

    let v: serde_json::Value =
        serde_json::from_str(&text_of(&result)).expect("valid JSON envelope");

    // Totals.
    assert_eq!(v["totals"]["commits"], 3, "three commits in window");
    assert_eq!(v["totals"]["added"], 10, "5+3+2 insertions");
    assert_eq!(v["totals"]["deleted"], 0, "no deletions (all new files)");
    assert_eq!(v["totals"]["projects"], 1, "one active project");
    assert_eq!(v["totals"]["active_days"], 3, "three distinct commit days");

    // Type mix carries feat/fix/docs, one each.
    let type_mix: std::collections::HashMap<String, i64> = v["totals"]["type_mix"]
        .as_array()
        .expect("type_mix array")
        .iter()
        .map(|p| (p[0].as_str().unwrap().to_string(), p[1].as_i64().unwrap()))
        .collect();
    assert_eq!(type_mix.get("feat"), Some(&1));
    assert_eq!(type_mix.get("fix"), Some(&1));
    assert_eq!(type_mix.get("docs"), Some(&1));

    // The single project, with its span and churn.
    let projects = v["projects"].as_array().expect("projects array");
    assert_eq!(projects.len(), 1);
    let p = &projects[0];
    assert_eq!(p["name"], "demo-proj");
    assert_eq!(p["commits"], 3);
    assert_eq!(p["added"], 10);
    assert_eq!(p["first"], "2026-05-05");
    assert_eq!(p["last"], "2026-05-20");
    // use_graph=off + no indexed history → enrichment reports unindexed.
    assert_eq!(p["enrichment"]["freshness"], "unindexed");

    // Normalized params are echoed back (the hardening contract).
    assert_eq!(v["normalized"]["author"], "Test Author");
    assert_eq!(v["normalized"]["format"], "json");
    assert_eq!(v["normalized"]["use_graph"], "off");
    assert_eq!(v["normalized"]["repos_scanned"], 1);

    // Markdown rendition is non-empty and carries the project + cadence.
    let md = server
        .call_tool_cli(
            "work_summary",
            serde_json::json!({
                "workspace_root": ws_s,
                "since": "2026-05-01",
                "until": "2026-06-01",
                "author": "Test Author",
                "format": "markdown",
                "use_graph": "off"
            }),
        )
        .await
        .expect("work_summary markdown call");
    let md_text = text_of(&md);
    assert!(md_text.contains("demo-proj"), "markdown names the project");
    assert!(md_text.contains("3 commits"), "markdown reports the count");

    // A bad format must reject at the boundary (no panic, clean error).
    let bad = server
        .call_tool_cli(
            "work_summary",
            serde_json::json!({"workspace_root": ws_s, "month": "2026-05", "format": "pdf"}),
        )
        .await;
    assert!(bad.is_err(), "unsupported format must be rejected");

    let _ = std::fs::remove_dir_all(&ws);
}
