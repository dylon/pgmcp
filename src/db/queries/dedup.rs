//! Shared worktree-dedup SQL fragments (the `worktree_dedup_clause`
//! const-fn + `DEDUP_3/4/5` clauses) used by the search/chunk readers to
//! collapse cross-worktree duplicates. Extracted from `queries.rs` (god-file split).
#![allow(unused_imports)]

use crate::db::queries::*;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

/// SQL fragment that filters cross-worktree duplicates of the same
/// relative path. When `$N::bool` is false (the default), `NOT $N` is
/// true and the OR short-circuits — no behavioural change. When true,
/// the NOT EXISTS sub-query removes any row whose project has a
/// lower-id sibling worktree (same `git_common_dir` or
/// `git_root_commits`) that also holds a file with the same
/// relative_path. Used by all four search tools (semantic, text, grep,
/// hybrid via text+semantic).
///
/// Aliases used by the embedding query: `f` for `indexed_files`,
/// `$N` for the bound `dedupe_worktrees::bool`. Caller must supply the
/// actual `$N` index in the SQL string substitution.
pub(crate) const fn worktree_dedup_clause(idx: u8) -> &'static str {
    // Keep the aliases minimal — `p_dup`/`f_dup`/`p_self` are local to
    // the sub-query and don't conflict with anything in the surrounding
    // SELECT (which uses `p`, `f`, `c`).
    match idx {
        3 => DEDUP_3,
        4 => DEDUP_4,
        5 => DEDUP_5,
        _ => panic!("worktree_dedup_clause: only $3..$5 supported"),
    }
}

pub(crate) const DEDUP_3: &str = "(NOT $3 OR NOT EXISTS (
    SELECT 1 FROM projects p_dup
    JOIN indexed_files f_dup ON f_dup.project_id = p_dup.id
    JOIN projects p_self ON p_self.id = f.project_id
    WHERE p_dup.id < p_self.id
      AND f_dup.relative_path = f.relative_path
      AND (
          (p_dup.git_common_dir IS NOT NULL AND p_self.git_common_dir IS NOT NULL
           AND p_dup.git_common_dir = p_self.git_common_dir)
          OR
          (p_dup.git_root_commits IS NOT NULL AND p_self.git_root_commits IS NOT NULL
           AND p_dup.git_root_commits = p_self.git_root_commits)
      )
))";
pub(crate) const DEDUP_4: &str = "(NOT $4 OR NOT EXISTS (
    SELECT 1 FROM projects p_dup
    JOIN indexed_files f_dup ON f_dup.project_id = p_dup.id
    JOIN projects p_self ON p_self.id = f.project_id
    WHERE p_dup.id < p_self.id
      AND f_dup.relative_path = f.relative_path
      AND (
          (p_dup.git_common_dir IS NOT NULL AND p_self.git_common_dir IS NOT NULL
           AND p_dup.git_common_dir = p_self.git_common_dir)
          OR
          (p_dup.git_root_commits IS NOT NULL AND p_self.git_root_commits IS NOT NULL
           AND p_dup.git_root_commits = p_self.git_root_commits)
      )
))";
pub(crate) const DEDUP_5: &str = "(NOT $5 OR NOT EXISTS (
    SELECT 1 FROM projects p_dup
    JOIN indexed_files f_dup ON f_dup.project_id = p_dup.id
    JOIN projects p_self ON p_self.id = f.project_id
    WHERE p_dup.id < p_self.id
      AND f_dup.relative_path = f.relative_path
      AND (
          (p_dup.git_common_dir IS NOT NULL AND p_self.git_common_dir IS NOT NULL
           AND p_dup.git_common_dir = p_self.git_common_dir)
          OR
          (p_dup.git_root_commits IS NOT NULL AND p_self.git_root_commits IS NOT NULL
           AND p_dup.git_root_commits = p_self.git_root_commits)
      )
))";
