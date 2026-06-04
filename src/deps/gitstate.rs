//! Live git-state reads for a project working tree (current branch, HEAD SHA,
//! dirty flag) and the stability check that drives the coordination gatekeeper.
//! Read-only — pgmcp never mutates git; it only observes (the trust boundary).

use std::process::Command;

/// A snapshot of a working tree's git state.
#[derive(Debug, Clone, Default)]
pub struct GitState {
    pub current_branch: Option<String>,
    pub head_sha: Option<String>,
    pub dirty: bool,
}

/// Read the live git state of a working tree. Best-effort: all `None`/`false` on
/// a non-repo or any git failure.
pub fn read_git_state(repo_path: &str) -> GitState {
    let current_branch = git_capture(repo_path, &["symbolic-ref", "--short", "HEAD"]);
    let head_sha = git_capture(repo_path, &["rev-parse", "HEAD"]);
    // `git status --porcelain` prints nothing for a clean tree.
    let dirty = git_capture(repo_path, &["status", "--porcelain"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    GitState {
        current_branch,
        head_sha,
        dirty,
    }
}

fn git_capture(repo_path: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// Whether the tree is "stable" for coordination purposes: on its configured
/// `stable_branch` (default `main`/`master`) **and** clean. This is the
/// gatekeeper signal — the only condition under which a dependency's pending
/// coordination requests are resolved (the dependent is unblocked).
pub fn is_stable(state: &GitState, stable_branch: Option<&str>) -> bool {
    if state.dirty {
        return false;
    }
    match (&state.current_branch, stable_branch) {
        (Some(b), Some(want)) => b == want,
        (Some(b), None) => b == "main" || b == "master",
        (None, _) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_stable_requires_clean_stable_branch() {
        let on_main_clean = GitState {
            current_branch: Some("main".into()),
            head_sha: Some("abc".into()),
            dirty: false,
        };
        assert!(is_stable(&on_main_clean, None));
        assert!(is_stable(&on_main_clean, Some("main")));
        assert!(!is_stable(&on_main_clean, Some("release"))); // wrong stable branch

        let on_main_dirty = GitState {
            dirty: true,
            ..on_main_clean.clone()
        };
        assert!(!is_stable(&on_main_dirty, None)); // dirty ⇒ unstable

        let on_feature = GitState {
            current_branch: Some("feat".into()),
            head_sha: Some("def".into()),
            dirty: false,
        };
        assert!(!is_stable(&on_feature, None)); // feature branch ⇒ unstable
        assert!(is_stable(&on_feature, Some("feat"))); // unless feat IS the stable branch

        let detached = GitState::default();
        assert!(!is_stable(&detached, None));
    }

    #[test]
    fn read_git_state_reads_branch_and_dirty_on_a_real_repo() {
        // Exercises the actual `git` subprocess reads on a throwaway repo. Skips
        // (best-effort, like the module) if git is unavailable.
        let dir = std::env::temp_dir().join(format!("pgmcp_gitstate_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir temp repo");
        let path = dir.to_str().expect("utf8 temp path");

        // Deterministic `main` default branch + a local identity, so the test does
        // not depend on the developer's global git config.
        let init_ok = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["-c", "init.defaultBranch=main", "init"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !init_ok {
            let _ = std::fs::remove_dir_all(&dir);
            eprintln!("skipping read_git_state test: git unavailable");
            return;
        }
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(path)
                .args(args)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            assert!(ok, "git {args:?} failed");
        };
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(dir.join("a.txt"), "hello").expect("write file");
        git(&["add", "-A"]);
        git(&["commit", "-m", "init", "--no-gpg-sign"]);

        let clean = read_git_state(path);
        assert_eq!(clean.current_branch.as_deref(), Some("main"), "on main");
        assert!(clean.head_sha.is_some(), "HEAD sha resolved");
        assert!(!clean.dirty, "a freshly-committed tree is clean");
        assert!(is_stable(&clean, None), "clean main ⇒ stable");
        assert!(is_stable(&clean, Some("main")));

        // An uncommitted edit makes it dirty (⇒ not stable).
        std::fs::write(dir.join("a.txt"), "changed").expect("modify file");
        let dirty = read_git_state(path);
        assert!(dirty.dirty, "a modified working tree is dirty");
        assert!(!is_stable(&dirty, None), "dirty ⇒ not stable");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
