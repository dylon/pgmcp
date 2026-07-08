//! Project-level queries (upsert/list/find-by-cwd/worktree-main
//! resolution/language-summary/cleanup). Extracted from `queries.rs` (god-file split).
#![allow(unused_imports)]

use crate::db::queries::*;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use sqlx::PgPool;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// TTL (seconds) for the cwd→project-id cache; `0` disables it. Set once at
/// daemon startup from `[clients] cwd_project_cache_ttl_secs`.
static CWD_PROJECT_CACHE_TTL_SECS: AtomicU64 = AtomicU64::new(30);

/// Process-wide cwd → (project-id, inserted-at) cache for [`find_project_id_by_cwd`].
static CWD_PROJECT_CACHE: LazyLock<DashMap<String, (Option<i32>, Instant)>> =
    LazyLock::new(DashMap::new);

/// Hard cap on distinct cached cwds; on overflow the cache is cleared (it rebuilds
/// cheaply). cwds are naturally bounded (project dirs + a handful of agent cwds),
/// so this only guards the pathological case — it can never grow without bound.
const CWD_PROJECT_CACHE_MAX: usize = 4096;

/// Set the cwd→project-id cache TTL (seconds); `0` disables the cache.
pub fn set_cwd_project_cache_ttl_secs(secs: u64) {
    CWD_PROJECT_CACHE_TTL_SECS.store(secs, Ordering::Relaxed);
}

// ============================================================================
// Project queries
// ============================================================================

/// Upsert a project (create or update).
///
/// `git_common_dir` and `git_root_commits` are the worktree-grouping
/// signals (see `src/indexer/git_indexer.rs::detect_git_common_dir` /
/// `detect_git_root_commits`). Both are optional and pass `None` for
/// non-git projects. Re-scans update the values via the ON CONFLICT
/// branch so adding/removing/cloning a worktree is reflected on the
/// next scan.
pub async fn upsert_project(
    pool: &PgPool,
    workspace_path: &str,
    path: &str,
    name: &str,
    git_common_dir: Option<&str>,
    git_root_commits: Option<&str>,
) -> Result<i32, sqlx::Error> {
    // `last_scanned_at = NOW()` is set on both INSERT and ON CONFLICT so
    // any file processed for a project bumps its freshness signal. The
    // `update_projects_scanned_by_workspace` helper catches the
    // no-file-change case (rescan finds nothing new) by issuing a bulk
    // UPDATE keyed on `workspace_path`.
    let row = sqlx::query_scalar::<_, i32>(
        "INSERT INTO projects (workspace_path, path, name, git_common_dir, git_root_commits, last_scanned_at)
         VALUES ($1, $2, $3, $4, $5, NOW())
         ON CONFLICT (path) DO UPDATE SET
            workspace_path = EXCLUDED.workspace_path,
            name = EXCLUDED.name,
            git_common_dir = EXCLUDED.git_common_dir,
            git_root_commits = EXCLUDED.git_root_commits,
            last_scanned_at = NOW()
         RETURNING id",
    )
    .bind(workspace_path)
    .bind(path)
    .bind(name)
    .bind(git_common_dir)
    .bind(git_root_commits)
    .fetch_one(pool)
    .await?;

    Ok(row)
}

/// List all projects with file counts.
pub async fn list_projects(pool: &PgPool) -> Result<Vec<ProjectInfo>, sqlx::Error> {
    let rows = sqlx::query_as::<_, ProjectInfo>(
        "SELECT p.id, p.workspace_path, p.path, p.name,
                p.git_common_dir, p.git_root_commits,
                p.discovered_at, p.last_scanned_at,
                COUNT(f.id) as file_count
         FROM projects p
         LEFT JOIN indexed_files f ON f.project_id = p.id
         GROUP BY p.id
         ORDER BY p.name",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct ProjectInfo {
    pub id: i32,
    pub workspace_path: String,
    pub path: String,
    pub name: String,
    /// Canonical absolute path of the shared `.git` directory. Two
    /// projects with the same value are worktrees of the same repo.
    /// `None` for non-git projects.
    pub git_common_dir: Option<String>,
    /// Sorted comma-joined list of root-commit SHAs. Two projects with
    /// the same value are sibling clones (or worktrees) of the same
    /// upstream repo. `None` when the project isn't a git repo or has
    /// no commits.
    pub git_root_commits: Option<String>,
    pub discovered_at: Option<DateTime<Utc>>,
    pub last_scanned_at: Option<DateTime<Utc>>,
    pub file_count: Option<i64>,
}

/// Lean variant of [`find_project_by_cwd`] returning only the project **id**.
///
/// The full variant carries a correlated `(SELECT COUNT(*) FROM indexed_files …)`
/// that costs ~1.2 s under concurrent write load (heap visibility fetches on the
/// very table being re-indexed) — and every hot caller that only needs the id
/// throws that count away. The high-frequency loops (the reactive ingest writer
/// at ~200 ms per distinct path, and the mcp-client-liveness sweep per client per
/// 30 s) use this id-only query instead, so they stop piling that count onto the
/// DB during a matview-refresh saturation window. The remaining `LIKE`-prefix
/// scan over ~99 project rows is sub-millisecond. Same longest-prefix semantics.
pub async fn find_project_id_by_cwd(pool: &PgPool, cwd: &str) -> Result<Option<i32>, sqlx::Error> {
    let ttl = CWD_PROJECT_CACHE_TTL_SECS.load(Ordering::Relaxed);
    if ttl > 0
        && let Some(hit) = CWD_PROJECT_CACHE.get(cwd)
    {
        let (id, at) = *hit;
        if at.elapsed().as_secs() < ttl {
            return Ok(id);
        }
    }
    let id = sqlx::query_scalar::<_, i32>(
        "SELECT p.id
         FROM projects p
         WHERE $1 = p.path
            OR $1 LIKE CASE
                 WHEN right(p.path, 1) = '/' THEN p.path || '%'
                 ELSE p.path || '/%'
               END
         ORDER BY LENGTH(p.path) DESC
         LIMIT 1",
    )
    .bind(cwd)
    .fetch_optional(pool)
    .await?;
    if ttl > 0 {
        if CWD_PROJECT_CACHE.len() >= CWD_PROJECT_CACHE_MAX {
            CWD_PROJECT_CACHE.clear();
        }
        CWD_PROJECT_CACHE.insert(cwd.to_string(), (id, Instant::now()));
    }
    Ok(id)
}

/// Find the project whose path is the longest prefix of a given directory.
/// Used by the `context` CLI subcommand to identify which project the user is in.
/// Carries `file_count`; hot callers that only need the id should use the lean
/// [`find_project_id_by_cwd`] instead.
pub async fn find_project_by_cwd(
    pool: &PgPool,
    cwd: &str,
) -> Result<Option<ProjectInfo>, sqlx::Error> {
    sqlx::query_as::<_, ProjectInfo>(
        "SELECT p.id, p.workspace_path, p.path, p.name,
                p.git_common_dir, p.git_root_commits,
                p.discovered_at, p.last_scanned_at,
                (SELECT COUNT(*) FROM indexed_files f WHERE f.project_id = p.id) AS file_count
         FROM projects p
         WHERE $1 = p.path
            OR $1 LIKE CASE
                 WHEN right(p.path, 1) = '/' THEN p.path || '%'
                 ELSE p.path || '/%'
               END
         ORDER BY LENGTH(p.path) DESC
         LIMIT 1",
    )
    .bind(cwd)
    .fetch_optional(pool)
    .await
}

/// Resolve which projects are the **main** of their worktree group.
///
/// pgmcp indexes ~50 projects, many of which are git worktrees of the same
/// upstream (e.g. `f1r3node/`, `f1r3node-cost-accounted-rho-calc/`,
/// `f1r3node-reified-rspaces/` all share `git_common_dir`). For cross-project
/// analyses (similarity, refactoring, dependency-health) we want one canonical
/// row per worktree group; otherwise feature-branch worktrees double-count.
///
/// Grouping rule: two projects are siblings if they share `git_common_dir`
/// (canonical) or — when that's NULL — `git_root_commits` (works for non-git
/// or detached worktrees). Projects with both NULL are singletons.
///
/// Within each group of size > 1, the **main** project is the one whose
/// directory basename is shortest (with lexicographic-shortest as a
/// tiebreaker). The user's convention is that feature-branch worktrees use
/// the form `<canonical>-<feature>`, so the canonical name is always a
/// strict prefix of every sibling — and therefore the shortest basename.
///
/// Returns the list of `project_id`s that are main, sorted ascending for
/// stable downstream pagination.
pub async fn select_main_worktree_projects(pool: &PgPool) -> Result<Vec<i32>, sqlx::Error> {
    #[derive(sqlx::FromRow)]
    struct Row {
        id: i32,
        path: String,
        git_common_dir: Option<String>,
        git_root_commits: Option<String>,
    }

    let rows: Vec<Row> =
        sqlx::query_as::<_, Row>("SELECT id, path, git_common_dir, git_root_commits FROM projects")
            .fetch_all(pool)
            .await?;

    let tuples: Vec<(i32, String, Option<String>, Option<String>)> = rows
        .into_iter()
        .map(|r| (r.id, r.path, r.git_common_dir, r.git_root_commits))
        .collect();

    Ok(pick_main_worktree_ids(&tuples))
}

/// Pure grouping logic — given `(id, path, git_common_dir, git_root_commits)`
/// rows, return the main project id of each worktree group. Extracted from
/// `select_main_worktree_projects` so the algorithm can be unit-tested without
/// a Postgres instance.
pub(crate) fn pick_main_worktree_ids(
    rows: &[(i32, String, Option<String>, Option<String>)],
) -> Vec<i32> {
    use std::collections::HashMap;

    let mut groups: HashMap<(String, String), Vec<(i32, String)>> = HashMap::new();
    let mut singletons: Vec<i32> = Vec::new();

    for (id, path, git_common_dir, git_root_commits) in rows {
        let basename = path
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(path)
            .to_string();
        match (git_common_dir.as_deref(), git_root_commits.as_deref()) {
            (None, None) => singletons.push(*id),
            (cd, rc) => {
                let key = (cd.unwrap_or("").to_string(), rc.unwrap_or("").to_string());
                groups.entry(key).or_default().push((*id, basename));
            }
        }
    }

    let mut main_ids = singletons;
    for (_, mut members) in groups {
        // Shortest basename wins; lexicographic tie-break; final tie-break by id.
        members.sort_by(|a, b| {
            worktree_basename_rank(&a.1)
                .cmp(&worktree_basename_rank(&b.1))
                .then_with(|| a.0.cmp(&b.0))
        });
        if let Some((id, _)) = members.into_iter().next() {
            main_ids.push(id);
        }
    }

    main_ids.sort();
    main_ids
}

/// Ranking key for the "main" of a worktree group: **shortest basename wins,
/// lexicographic as the tiebreak**. Extracted so the convention lives in exactly
/// one pure, tested place: [`pick_main_worktree_ids`] applies it per group at
/// query time, and [`project_main_path_name`] resolves the same canonical
/// checkout at assignment time (see its docs for how the two agree).
fn worktree_basename_rank(basename: &str) -> (usize, &str) {
    (basename.len(), basename)
}

/// Assignment-time project identity for a resolved git root: the `(path, name)`
/// under which the root should be upserted so that all linked worktrees of one
/// upstream collapse onto a **single main-checkout project row** — no
/// per-worktree row explosion, and correct cross-project analysis.
///
/// The main checkout is the directory that **owns the shared `.git`**: the
/// parent of `git_common_dir`, which `detect_git_common_dir` canonicalizes to
/// `<main>/.git` for the main checkout AND every linked worktree of it. So:
///
/// - a **linked worktree** (its `.git` is a FILE pointing at the shared dir)
///   projects onto its main checkout;
/// - a **normal checkout** or an **independent fork** (its `.git` is its OWN
///   directory) has `parent(git_common_dir) == own_path`, so it projects onto
///   ITSELF and stays its own project — a distinct fork is never merged;
/// - a **non-git directory** (`git_common_dir == None`) keeps its own path.
///
/// `git_common_dir` is used directly (rather than re-deriving via the
/// shortest-basename heuristic) because the owner is the *group-invariant*
/// target: every worktree of one upstream maps to the same checkout, which is
/// exactly what guarantees the collapse. Under the user's `<canonical>-<feature>`
/// worktree-naming convention that owner is precisely the shortest-basename
/// member [`pick_main_worktree_ids`] selects at query time (via
/// [`worktree_basename_rank`]), so assignment-time identity and query-time
/// grouping agree. Only a `.git`-named common dir is unwrapped; anything else
/// (e.g. a bare-repo common dir) falls back to `own_path` so a checkout is never
/// mis-attributed.
// NOTE: not wired into assignment right now. Assignment uses PER-WORKTREE project
// identity (see src/embed/pool.rs): each resolved git root — including a linked
// worktree — is its own project, and cross-project tools default to the main via
// `pick_main_worktree_ids` at query time. Map-to-main (this helper) was reverted
// from the assignment path because collapsing worktrees onto their main inflates
// the main's RAW source-row count past the fuzzy `skip-oversize` threshold (the
// trie dedups by name but the row count double-counts worktree files), which would
// drop a heavily-worktree'd main's fuzzy trie. This validated helper is retained
// for a future map-to-main path that also dedups worktree files at index time.
#[allow(dead_code)]
pub(crate) fn project_main_path_name(
    own_path: &Path,
    git_common_dir: Option<&str>,
) -> (PathBuf, String) {
    let main_path = git_common_dir
        .map(Path::new)
        .filter(|gcd| gcd.file_name() == Some(std::ffi::OsStr::new(".git")))
        .and_then(Path::parent)
        // A main checkout / independent fork owns its OWN `.git`, so the owner
        // equals `own_path` — keep it as its own project. Only a DIFFERENT owner
        // (a linked worktree's upstream) remaps.
        .filter(|owner| *owner != own_path)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| own_path.to_path_buf());
    let name = main_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| main_path.to_string_lossy().into_owned());
    (main_path, name)
}

#[cfg(test)]
mod worktree_tests {
    use super::pick_main_worktree_ids;

    /// Helper to build the (id, path, common_dir, root_commits) tuple used by
    /// the resolver.
    fn row(
        id: i32,
        path: &str,
        common_dir: Option<&str>,
        root_commits: Option<&str>,
    ) -> (i32, String, Option<String>, Option<String>) {
        (
            id,
            path.to_string(),
            common_dir.map(str::to_string),
            root_commits.map(str::to_string),
        )
    }

    #[test]
    fn singleton_project_is_always_main() {
        let rows = vec![row(1, "/ws/foo", None, None)];
        assert_eq!(pick_main_worktree_ids(&rows), vec![1]);
    }

    #[test]
    fn worktree_group_sharing_git_common_dir_picks_shortest_basename() {
        // `f1r3node` < `f1r3node-cost-accounted-rho-calc` < `f1r3node-reified-rspaces`
        let rows = vec![
            row(
                10,
                "/ws/f1r3node-reified-rspaces",
                Some("/ws/f1r3node/.git"),
                None,
            ),
            row(11, "/ws/f1r3node", Some("/ws/f1r3node/.git"), None),
            row(
                12,
                "/ws/f1r3node-cost-accounted-rho-calc",
                Some("/ws/f1r3node/.git"),
                None,
            ),
        ];
        assert_eq!(pick_main_worktree_ids(&rows), vec![11]);
    }

    #[test]
    fn worktree_group_sharing_root_commits_when_common_dir_is_null() {
        // `MeTTa-Compiler/.git` is null for the worktree clones in the
        // user's setup; they're still grouped via `git_root_commits`.
        let rows = vec![
            row(20, "/ws/MeTTa-Compiler-JIT", None, Some("abc123")),
            row(21, "/ws/MeTTa-Compiler", None, Some("abc123")),
            row(22, "/ws/MeTTa-Compiler-PR-63", None, Some("abc123")),
        ];
        assert_eq!(pick_main_worktree_ids(&rows), vec![21]);
    }

    #[test]
    fn projects_with_both_keys_null_each_become_their_own_group() {
        // Two unrelated non-git projects must NOT collapse into one group;
        // each stays a singleton.
        let rows = vec![
            row(30, "/ws/alpha", None, None),
            row(31, "/ws/beta", None, None),
        ];
        let mut got = pick_main_worktree_ids(&rows);
        got.sort();
        assert_eq!(got, vec![30, 31]);
    }

    #[test]
    fn lexicographic_tiebreak_when_basenames_equal_length() {
        // Equal-length basenames (5 chars each): pick the lexicographically smallest.
        let rows = vec![
            row(40, "/ws/delta", Some("/shared/.git"), None),
            row(41, "/ws/alpha", Some("/shared/.git"), None),
        ];
        // Both "delta" and "alpha" are 5 chars; "alpha" < "delta" lexicographically.
        assert_eq!(pick_main_worktree_ids(&rows), vec![41]);
    }

    #[test]
    fn id_tiebreak_when_basename_identical() {
        // Two rows with identical basenames (path collision is degenerate
        // but possible from a stale rescan) — fall through to id ascending.
        let rows = vec![
            row(50, "/ws-a/dup", Some("/shared/.git"), None),
            row(49, "/ws-b/dup", Some("/shared/.git"), None),
        ];
        assert_eq!(pick_main_worktree_ids(&rows), vec![49]);
    }

    #[test]
    fn singletons_and_groups_combine_in_one_call() {
        let rows = vec![
            // Worktree group A: main is 100 ("rust")
            row(100, "/ws/rust", Some("/a/.git"), None),
            row(101, "/ws/rust-feature-x", Some("/a/.git"), None),
            // Singleton project
            row(200, "/ws/lonely", None, None),
            // Worktree group B keyed on root_commits: main is 301 ("py")
            row(300, "/ws/py-experimental", None, Some("xyz")),
            row(301, "/ws/py", None, Some("xyz")),
        ];
        assert_eq!(pick_main_worktree_ids(&rows), vec![100, 200, 301]);
    }

    #[test]
    fn trailing_slash_in_path_does_not_break_basename() {
        let rows = vec![row(60, "/ws/foo/", Some("/a/.git"), None)];
        // Basename of "/ws/foo/" is "foo".
        assert_eq!(pick_main_worktree_ids(&rows), vec![60]);
    }

    #[test]
    fn empty_input_returns_empty() {
        let rows: Vec<(i32, String, Option<String>, Option<String>)> = Vec::new();
        assert!(pick_main_worktree_ids(&rows).is_empty());
    }
}

#[cfg(test)]
mod project_identity_tests {
    use super::project_main_path_name;
    use std::path::{Path, PathBuf};

    /// C3: a linked worktree (its `.git` is a FILE whose canonical common dir is
    /// the MAIN checkout's `.git`) projects onto the main checkout's path/name,
    /// so every worktree of one upstream collapses to a single project row.
    #[test]
    fn git_file_worktree_projects_onto_its_main_checkout() {
        // `f1r3node-reified-rspaces` is a worktree of `f1r3node`; both resolve
        // `git_common_dir` to `/ws/f1r3node/.git`.
        let (path, name) = project_main_path_name(
            Path::new("/ws/f1r3node-reified-rspaces"),
            Some("/ws/f1r3node/.git"),
        );
        assert_eq!(path, PathBuf::from("/ws/f1r3node"));
        assert_eq!(name, "f1r3node");
    }

    /// C3: an independent fork / normal checkout (its `.git` is its OWN
    /// directory, so `git_common_dir` points under itself) stays its own
    /// project — a distinct fork is never merged into another.
    #[test]
    fn git_dir_fork_projects_onto_itself() {
        let (path, name) = project_main_path_name(
            Path::new("/ws/MeTTa-Compiler-fork"),
            Some("/ws/MeTTa-Compiler-fork/.git"),
        );
        assert_eq!(path, PathBuf::from("/ws/MeTTa-Compiler-fork"));
        assert_eq!(name, "MeTTa-Compiler-fork");
    }

    /// A plain main checkout resolves to itself (identity is unchanged from the
    /// pre-C3 behavior for normal git-rooted projects).
    #[test]
    fn main_checkout_projects_onto_itself() {
        let (path, name) =
            project_main_path_name(Path::new("/ws/f1r3node"), Some("/ws/f1r3node/.git"));
        assert_eq!(path, PathBuf::from("/ws/f1r3node"));
        assert_eq!(name, "f1r3node");
    }

    /// C3: a non-git directory (no `git_common_dir`) keeps its own path/name —
    /// C2 gives it a bounded per-directory project; it is never a shared default.
    #[test]
    fn non_git_dir_keeps_its_own_identity() {
        let (path, name) = project_main_path_name(Path::new("/ws/loose-notes"), None);
        assert_eq!(path, PathBuf::from("/ws/loose-notes"));
        assert_eq!(name, "loose-notes");
    }

    /// A common dir that is NOT `.git`-named (e.g. a bare repo) does not unwrap;
    /// the project keeps its own path so a checkout is never mis-attributed.
    #[test]
    fn non_dotgit_common_dir_falls_back_to_own_path() {
        let (path, name) =
            project_main_path_name(Path::new("/ws/checkout"), Some("/ws/bare-repo.git"));
        assert_eq!(path, PathBuf::from("/ws/checkout"));
        assert_eq!(name, "checkout");
    }
}

/// Returns language breakdown (language, count) for a project, ordered by count descending.
pub async fn language_summary(
    pool: &PgPool,
    project_name: &str,
) -> Result<Vec<LanguageCount>, sqlx::Error> {
    sqlx::query_as::<_, LanguageCount>(
        "SELECT f.language, COUNT(*) as count
         FROM indexed_files f
         JOIN projects p ON f.project_id = p.id
         WHERE p.name = $1
         GROUP BY f.language
         ORDER BY count DESC",
    )
    .bind(project_name)
    .fetch_all(pool)
    .await
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct LanguageCount {
    pub language: String,
    pub count: i64,
}

/// Update last_scanned_at for a project.
pub async fn update_project_scanned(pool: &PgPool, project_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE projects SET last_scanned_at = NOW() WHERE id = $1")
        .bind(project_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Bulk-update `last_scanned_at = NOW()` for every project whose
/// `workspace_path` matches the given string. Used by the scanner +
/// rescan paths to mark "this workspace was fully walked at NOW()",
/// even when no files changed (so no `upsert_project` was called and
/// the per-file `last_scanned_at` update never fired).
///
/// Returns the number of rows updated.
pub async fn update_projects_scanned_by_workspace(
    pool: &PgPool,
    workspace_path: &str,
) -> Result<u64, sqlx::Error> {
    let result =
        sqlx::query("UPDATE projects SET last_scanned_at = NOW() WHERE workspace_path = $1")
            .bind(workspace_path)
            .execute(pool)
            .await?;
    Ok(result.rows_affected())
}

// ============================================================================
// Completion queries
// ============================================================================

/// List all distinct project names (for completions).
pub async fn list_project_names(pool: &PgPool) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar::<_, String>("SELECT DISTINCT name FROM projects ORDER BY name")
        .fetch_all(pool)
        .await
}

/// List all distinct languages from indexed files (for completions).
pub async fn list_languages(pool: &PgPool) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar::<_, String>("SELECT DISTINCT language FROM indexed_files ORDER BY language")
        .fetch_all(pool)
        .await
}

/// Get all projects that have git history indexing enabled (via .pgmcp.toml).
/// Returns (project_id, project_path) pairs.
pub async fn get_git_enabled_projects(pool: &PgPool) -> Result<Vec<(i32, String)>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (i32, String)>("SELECT id, path FROM projects ORDER BY name")
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Delete all projects whose `workspace_path` matches the given path.
/// CASCADE handles `indexed_files` → `file_chunks`, `git_commits` → `git_commit_chunks`.
/// Returns the number of projects deleted.
pub async fn delete_projects_by_workspace(
    pool: &PgPool,
    workspace_path: &str,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM projects WHERE workspace_path = $1")
        .bind(workspace_path)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

/// Delete projects that have zero indexed files (orphaned after stale file removal).
pub async fn cleanup_orphaned_projects(pool: &PgPool) -> Result<u64, sqlx::Error> {
    // Corpus-scale: the NOT EXISTS anti-join scans every project against
    // indexed_files and the DELETE cascades to git_commits/git_commit_chunks,
    // which can exceed the pool's 30 s default; lift the timeout for the tx.
    let mut tx = crate::db::pool::begin_heavy(pool, "0", "stale-cleanup").await?;
    let result = sqlx::query(
        "DELETE FROM projects p
         WHERE NOT EXISTS (SELECT 1 FROM indexed_files f WHERE f.project_id = p.id)",
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result.rows_affected())
}
