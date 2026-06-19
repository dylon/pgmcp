//! `bug-gate` subcommand: the boyscout enforcement gate (ADR-022, engineering
//! principle #2).
//!
//! Fails (exit 1) when an open `kind='bug'` work-item is anchored — via
//! `work_item_code_anchor` — to a file touched by the current diff, so the author
//! is forced to fix the bug before pushing changes to the same code. Pass
//! `--warn-only` to report without failing.
//!
//! Self-skips **loudly** (exit 0, with a logged warning + a printed SKIPPED line)
//! when run outside a git work tree or when the database is unavailable: a
//! missing VCS/DB must never silently disable the gate, nor block a contributor
//! who simply lacks a local Postgres. This mirrors the no-silent-skip discipline
//! of `scripts/verify.sh`'s test-DB preflight.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::Config;
use crate::db;

pub async fn run(
    config_override: Option<&Path>,
    cwd: Option<PathBuf>,
    base: Option<String>,
    warn_only: bool,
    limit: i64,
) -> anyhow::Result<()> {
    let config = Config::load(config_override)?;
    crate::logging::init_cli_with_config(Some(&config));

    let repo_dir = match cwd {
        Some(p) => p,
        None => std::env::current_dir()?,
    };

    let paths = match collect_touched_files(&repo_dir, base.as_deref()) {
        Some(p) => p,
        None => {
            tracing::warn!(
                dir = %repo_dir.display(),
                "bug-gate: not a git work tree (or git unavailable); skipping"
            );
            println!("bug-gate: SKIPPED (not a git work tree)");
            return Ok(());
        }
    };
    if paths.is_empty() {
        println!("bug-gate: no changed files; nothing to check.");
        return Ok(());
    }

    let pool = match db::pool::create_pool(&config.database).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "bug-gate: database unavailable; skipping");
            println!("bug-gate: SKIPPED (database unavailable: {e})");
            return Ok(());
        }
    };

    let bugs = db::queries::open_bugs_anchored_to_paths(&pool, &paths, limit).await?;
    if bugs.is_empty() {
        println!(
            "bug-gate: ✓ no open bugs anchored to {} changed file(s).",
            paths.len()
        );
        return Ok(());
    }

    eprintln!(
        "bug-gate: {} open bug(s) anchored to files in this diff — fix them \
         (boyscout rule, ADR-022):",
        bugs.len()
    );
    for b in &bugs {
        eprintln!(
            "  [{}] {} ({})  {}  — {}",
            b.severity.as_deref().unwrap_or("?"),
            b.public_id,
            b.status,
            b.relative_path,
            b.title,
        );
    }

    if warn_only {
        eprintln!("bug-gate: --warn-only set; reporting only, not failing.");
        Ok(())
    } else {
        anyhow::bail!(
            "bug-gate: {} unresolved bug(s) anchored to changed files (use --warn-only to \
             downgrade to advisory)",
            bugs.len()
        )
    }
}

/// The union of uncommitted working-tree changes (staged + unstaged, vs `HEAD`)
/// and — when `base` is given — committed changes since `base` (`base..HEAD`).
/// Returns `None` if `repo_dir` is not a git work tree (or git is unavailable),
/// which the caller treats as a loud self-skip.
fn collect_touched_files(repo_dir: &Path, base: Option<&str>) -> Option<Vec<String>> {
    let probe = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .ok()?;
    if !probe.status.success() {
        return None;
    }

    let mut set: BTreeSet<String> = BTreeSet::new();
    for f in git_name_only(repo_dir, &["diff", "--name-only", "HEAD"]) {
        set.insert(f);
    }
    if let Some(b) = base {
        let range = format!("{b}..HEAD");
        for f in git_name_only(repo_dir, &["diff", "--name-only", &range]) {
            set.insert(f);
        }
    }
    Some(set.into_iter().collect())
}

/// Run `git -C <repo_dir> <args>` and return the non-empty trimmed stdout lines
/// (a `--name-only` file list). Empty on any failure — a transient git error
/// must not crash the gate.
fn git_name_only(repo_dir: &Path, args: &[&str]) -> Vec<String> {
    Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}
