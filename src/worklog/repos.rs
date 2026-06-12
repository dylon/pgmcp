//! Canonical-repo enumeration for a workspace work-summary.
//!
//! Reuses the existing worktree dedup in `db::queries::projects`
//! (`pick_main_worktree_ids`: group by `git_common_dir` else `git_root_commits`,
//! keep the shortest basename) so feature-branch worktrees of one upstream never
//! double-count, while genuinely distinct projects (e.g. `f1r3node` vs
//! `f1r3node-rust`, which have different root commits) stay separate.

use std::collections::HashSet;

use rmcp::ErrorData as McpError;

use crate::context::SystemContext;
use crate::db::queries::{list_projects, pick_main_worktree_ids};
use crate::mcp::tools::sota_helpers::pool_or_err;

/// One canonical repository to summarize (its worktree group collapsed to main).
#[derive(Debug, Clone)]
pub struct Repo {
    pub project_id: i32,
    pub name: String,
    pub path: String,
}

/// Enumerate the canonical indexed repos whose path is under `workspace_root`,
/// capped at `max_repos` (after canonicalization, sorted by name for stability).
pub async fn canonical_repos(
    ctx: &SystemContext,
    workspace_root: &str,
    max_repos: usize,
) -> Result<Vec<Repo>, McpError> {
    let pool = pool_or_err(ctx)?;
    let all = list_projects(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("list_projects failed: {e}"), None))?;

    // Canonicalize across ALL projects first (so worktree groups stay intact),
    // then keep the mains located under workspace_root.
    let tuples: Vec<(i32, String, Option<String>, Option<String>)> = all
        .iter()
        .map(|p| {
            (
                p.id,
                p.path.clone(),
                p.git_common_dir.clone(),
                p.git_root_commits.clone(),
            )
        })
        .collect();
    let main_ids: HashSet<i32> = pick_main_worktree_ids(&tuples).into_iter().collect();

    let root = normalize_dir(workspace_root);
    let mut repos: Vec<Repo> = all
        .into_iter()
        .filter(|p| main_ids.contains(&p.id))
        .filter(|p| path_under(&p.path, &root))
        .map(|p| Repo {
            project_id: p.id,
            name: p.name,
            path: p.path,
        })
        .collect();
    repos.sort_by(|a, b| a.name.cmp(&b.name));
    repos.truncate(max_repos);
    Ok(repos)
}

/// Expand a leading `~/` to `$HOME` and strip a trailing slash.
pub fn normalize_dir(p: &str) -> String {
    let p = p.trim();
    let expanded = if let Some(rest) = p.strip_prefix("~/") {
        match std::env::var("HOME") {
            Ok(home) => format!("{}/{}", home.trim_end_matches('/'), rest),
            Err(_) => p.to_string(),
        }
    } else if p == "~" {
        std::env::var("HOME").unwrap_or_else(|_| p.to_string())
    } else {
        p.to_string()
    };
    expanded.trim_end_matches('/').to_string()
}

/// True when `path` is `root` itself or a descendant directory of it.
fn path_under(path: &str, root: &str) -> bool {
    let path = path.trim_end_matches('/');
    path == root || path.starts_with(&format!("{root}/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_under_matches_self_and_descendants_only() {
        assert!(path_under("/ws/f1r3fly.io", "/ws/f1r3fly.io"));
        assert!(path_under("/ws/f1r3fly.io/pgmcp", "/ws/f1r3fly.io"));
        assert!(path_under("/ws/f1r3fly.io/pgmcp/", "/ws/f1r3fly.io"));
        // A sibling sharing a name prefix must NOT match.
        assert!(!path_under("/ws/f1r3fly.io-backup", "/ws/f1r3fly.io"));
        assert!(!path_under("/ws/other", "/ws/f1r3fly.io"));
    }

    #[test]
    fn normalize_dir_strips_trailing_slash() {
        assert_eq!(normalize_dir("/a/b/"), "/a/b");
        assert_eq!(normalize_dir("  /a/b  "), "/a/b");
    }
}
