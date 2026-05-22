//! `SyntheticRoots` + `effective_workspace_paths` — extracted from the
//! parent `scanner.rs` as part of the D.2 god-file split.

use std::path::PathBuf;

use crate::config::Config;

/// Resolved paths for the four synthetic project directories that
/// `scan_workspaces` auto-discovers. Computed once by the caller —
/// production uses `SyntheticRoots::from_home()` at scan-start, tests
/// construct fields explicitly to point at tempdirs.
///
/// Decoupling auto-discovery from `dirs::home_dir()` lets tests run in
/// parallel without racing on `std::env::set_var("HOME", …)`. Before
/// this struct existed, tests had to mutate process-global `$HOME` to
/// redirect `Config::*_dir()`, which produced an indefinite deadlock
/// when concurrent tests clobbered each other's overrides.
#[derive(Debug, Default, Clone)]
pub struct SyntheticRoots {
    pub claude: Option<PathBuf>,
    pub codex: Option<PathBuf>,
    pub papers: Option<PathBuf>,
    pub documents: Option<PathBuf>,
}

impl SyntheticRoots {
    /// Resolve all four roots from `$HOME` (via `dirs::home_dir()`
    /// through the existing `Config::*_dir()` helpers). Production
    /// callers invoke this once per scan cycle.
    pub fn from_home() -> Self {
        Self {
            claude: Config::claude_dir(),
            codex: Config::codex_dir(),
            papers: Config::papers_dir(),
            documents: Config::documents_dir(),
        }
    }

    /// All-`None`: no synthetic auto-discovery. Tests use this when
    /// they only want the regular workspace walker to run.
    // `pgmcp-testing` (a separate workspace member) is the sole caller,
    // so the dead-code lint can't see usage when linting the main crate.
    #[allow(dead_code)]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Iterate the synthetic-root paths that exist on disk, as
    /// `(name, path)` pairs. Used by `effective_workspace_paths` and
    /// by callers that need to log which roots were discovered.
    pub fn present(&self) -> impl Iterator<Item = (&'static str, &PathBuf)> {
        [
            ("claude", self.claude.as_ref()),
            ("codex", self.codex.as_ref()),
            ("papers", self.papers.as_ref()),
            ("documents", self.documents.as_ref()),
        ]
        .into_iter()
        .filter_map(|(name, opt)| opt.filter(|p| p.is_dir()).map(|p| (name, p)))
    }
}

/// Union of `config.workspace.paths` and any existing synthetic roots
/// (`~/.claude`, `~/.codex`, `~/Papers`, `~/Documents` — whichever are
/// present on disk). This is the canonical set of "directories pgmcp
/// cares about" and is consumed by BOTH the initial scanner
/// (`scan_workspaces`) and the inotify watcher (`start_watching`).
///
/// Without this unification, synthetic roots are populated by the
/// initial scan but receive no live watch — every edit to a Claude
/// memory file or Papers/PDF goes uncaught until the next daemon
/// restart. Live coverage for synthetic roots was Bug B in the
/// 2026-05-21 staleness investigation.
///
/// Duplicates are removed (a synthetic-root path that also appears in
/// `config.workspace.paths` is only emitted once).
pub fn effective_workspace_paths(config: &Config, roots: &SyntheticRoots) -> Vec<String> {
    let mut paths: Vec<String> = config.workspace.paths.clone();
    let mut seen: std::collections::HashSet<String> = paths.iter().cloned().collect();
    for (_name, path) in roots.present() {
        let s = path.to_string_lossy().into_owned();
        if seen.insert(s.clone()) {
            paths.push(s);
        }
    }
    paths
}
