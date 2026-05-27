//! Full workspace scanner using ignore::WalkParallel.
//!
//! Discovers project roots via .git/ directory heuristic.
//! Respects .gitignore files.
//! Auto-discovers agent homes such as `~/.claude/` and `~/.codex/` as
//! synthetic projects.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crossbeam_channel::Sender;
use dashmap::DashMap;
use ignore::WalkBuilder;
use tracing::{debug, info};

use crate::config::{self, Config};

mod synthetic_roots;
pub use synthetic_roots::*;

/// Read the set of file extensions present in `dir`. Used to apply
/// contextual extension rules (currently `.cfg` → `tlaplus` when a
/// sibling `.tla` exists in the same directory). Returns an empty set
/// when `dir` cannot be read.
fn sibling_extensions(dir: &Path) -> HashSet<String> {
    let mut out = HashSet::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Some(ext) = entry.path().extension().and_then(|e| e.to_str()) {
                out.insert(ext.to_string());
            }
        }
    }
    out
}

/// Inclusion gate that honours both the path-based extension allowlist
/// and the directory-aware contextual rules. For every extension other
/// than `.cfg` the gate uses the path-only fast path (no `readdir` cost);
/// `.cfg` triggers a sibling-extension scan to decide whether the file is
/// a TLA+ TLC config in a `.tla` project directory.
fn is_configured_path(config: &Config, path: &Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if ext != "cfg" {
        return config.indexer.is_configured_extension(path);
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let siblings = sibling_extensions(parent);
    config
        .indexer
        .is_configured_extension_in_context(path, &siblings)
}

/// Directories inside `~/.claude/` that should be excluded from indexing
/// (noise: telemetry, debug logs, cache, binary snapshots, etc.).
const CLAUDE_DIR_EXCLUDES: &[&str] = &[
    "debug",
    "shell-snapshots",
    "paste-cache",
    "cache",
    "backups",
    "plugins",
    "session-env",
    "statsig",
    "telemetry",
    "todos",
    "downloads",
    ".credentials.json",
    "stats-cache.json",
    "mcp-needs-auth-cache.json",
];

/// LaTeX build artifacts and OS noise to exclude under `~/Papers/`. The
/// patterns follow the same suffix/prefix matching convention used by
/// the global `exclude_patterns`: a leading `*` matches a suffix; anything
/// else is treated as a path-component substring.
const PAPERS_DIR_EXCLUDES: &[&str] = &[
    "*.aux",
    "*.log",
    "*.out",
    "*.toc",
    "*.lof",
    "*.lot",
    "*.synctex.gz",
    "*.fls",
    "*.fdb_latexmk",
    "*.bbl",
    "*.blg",
    "*.nav",
    "*.snm",
    "*.vrb",
    "*.idx",
    "*.ind",
    "*.ilg",
    "*.bak",
    ".~lock.", // LibreOffice lockfiles: ".~lock.<name>#"
    "Trash/",
    ".cache/",
    "tmp/",
];

/// Office-app temp files, downloads, and trash to exclude under
/// `~/Documents/`.
const DOCUMENTS_DIR_EXCLUDES: &[&str] = &[
    "*.bak",
    ".~lock.", // LibreOffice lockfiles
    "Trash/",
    ".cache/",
    "tmp/",
    ".DS_Store",
    "*.lnk",
];

/// Scan all configured workspace paths and submit files for indexing.
/// Also walks each of the synthetic project directories in `roots`
/// (typically `~/.claude/`, `~/.codex/`, `~/Papers/`, `~/Documents/`
/// in production; explicit tempdir paths in tests).
pub fn scan_workspaces(
    config: &Config,
    roots: &SyntheticRoots,
    file_tx: Sender<PathBuf>,
    project_roots: &DashMap<PathBuf, ProjectRoot>,
    project_overrides: &DashMap<PathBuf, config::ProjectOverride>,
) {
    for workspace_path in &config.workspace.paths {
        let workspace = Path::new(workspace_path);
        if !workspace.exists() {
            tracing::warn!(path = %workspace_path, "Workspace path does not exist");
            continue;
        }

        info!(path = %workspace_path, "Scanning workspace");
        scan_single_workspace(
            workspace,
            workspace_path,
            config,
            &file_tx,
            project_roots,
            project_overrides,
        );
    }

    // Scan project-level .claude/ directories
    let project_claude_dirs: Vec<(PathBuf, String)> = project_roots
        .iter()
        .filter_map(|entry| {
            let claude_subdir = entry.key().join(".claude");
            if claude_subdir.is_dir() {
                Some((claude_subdir, entry.value().workspace_path.clone()))
            } else {
                None
            }
        })
        .collect();

    for (claude_subdir, workspace_path) in project_claude_dirs {
        let subdir_str = claude_subdir.to_string_lossy().into_owned();
        info!(path = %subdir_str, "Scanning project-level .claude/ directory");
        scan_claude_dir(&claude_subdir, &workspace_path, config, &file_tx);
    }

    // Auto-discover ~/.claude/ if it was supplied
    if let Some(claude_dir) = roots.claude.as_ref() {
        let claude_path_str = claude_dir.to_string_lossy().into_owned();
        info!(path = %claude_path_str, "Auto-discovered ~/.claude/ directory");

        // Register as a synthetic project root (no .git/ needed)
        project_roots.insert(
            claude_dir.clone(),
            ProjectRoot {
                workspace_path: claude_path_str.clone(),
                name: "claude".into(),
            },
        );

        scan_claude_dir(claude_dir, &claude_path_str, config, &file_tx);
    }

    // Auto-discover ~/.codex/ if it was supplied. Codex contains credentials,
    // sqlite state, caches, and shell snapshots, so this path is allow-listed
    // instead of broadly indexing every configured extension.
    if let Some(codex_dir) = roots.codex.as_ref() {
        let codex_path_str = codex_dir.to_string_lossy().into_owned();
        info!(path = %codex_path_str, "Auto-discovered ~/.codex/ directory");

        project_roots.insert(
            codex_dir.clone(),
            ProjectRoot {
                workspace_path: codex_path_str.clone(),
                name: "codex".into(),
            },
        );

        scan_codex_dir(codex_dir, &codex_path_str, config, &file_tx);
    }

    // Auto-discover ~/Papers/ as a synthetic project. Honors a `.pgmcp.toml`
    // placed directly in the directory.
    if let Some(papers_dir) = roots.papers.as_ref() {
        let path_str = papers_dir.to_string_lossy().into_owned();
        info!(path = %path_str, "Auto-discovered ~/Papers/ directory");

        let name = papers_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Papers".into());
        project_roots.insert(
            papers_dir.clone(),
            ProjectRoot {
                workspace_path: path_str.clone(),
                name,
            },
        );
        if let Some(ovr) = config::ProjectOverride::load(papers_dir) {
            project_overrides.insert(papers_dir.clone(), ovr);
        }

        let override_snapshot = project_overrides.get(papers_dir).map(|r| r.value().clone());
        scan_papers_dir(
            papers_dir,
            &path_str,
            config,
            &file_tx,
            override_snapshot.as_ref(),
        );
    }

    // Auto-discover ~/Documents/ symmetric with ~/Papers/.
    if let Some(documents_dir) = roots.documents.as_ref() {
        let path_str = documents_dir.to_string_lossy().into_owned();
        info!(path = %path_str, "Auto-discovered ~/Documents/ directory");

        let name = documents_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Documents".into());
        project_roots.insert(
            documents_dir.clone(),
            ProjectRoot {
                workspace_path: path_str.clone(),
                name,
            },
        );
        if let Some(ovr) = config::ProjectOverride::load(documents_dir) {
            project_overrides.insert(documents_dir.clone(), ovr);
        }

        let override_snapshot = project_overrides
            .get(documents_dir)
            .map(|r| r.value().clone());
        scan_documents_dir(
            documents_dir,
            &path_str,
            config,
            &file_tx,
            override_snapshot.as_ref(),
        );
    }
}

/// Scan a single workspace directory.
pub(crate) fn scan_single_workspace(
    workspace: &Path,
    workspace_path: &str,
    config: &Config,
    file_tx: &Sender<PathBuf>,
    project_roots: &DashMap<PathBuf, ProjectRoot>,
    project_overrides: &DashMap<PathBuf, config::ProjectOverride>,
) {
    // If the workspace path itself is hidden (starts with '.'), allow hidden files
    let workspace_is_hidden = workspace
        .file_name()
        .map(|n| n.to_string_lossy().starts_with('.'))
        .unwrap_or(false);

    let mut builder = WalkBuilder::new(workspace);
    builder.hidden(!workspace_is_hidden); // Skip hidden unless workspace is hidden
    builder.git_ignore(true);
    builder.git_global(true);
    builder.git_exclude(true);

    // Add custom exclude patterns
    for pattern in &config.indexer.exclude_patterns {
        builder.add_custom_ignore_filename(".pgmcpignore");
        let mut override_builder = ignore::overrides::OverrideBuilder::new(workspace);
        let _ = override_builder.add(&format!("!{}", pattern));
    }

    // Walk the directory tree in PARALLEL. The `ignore` crate fans the
    // traversal + gitignore matching across threads; project-root detection
    // (DashMap) and file submission (crossbeam Sender) are thread-safe, and the
    // downstream consumer applies the per-file metadata skip independently of
    // order. NOTE (data-driven): the walk is not the cold-start bottleneck —
    // embedding throughput (`embeddings.pool_size` × GPU batch) is — and the
    // walk already runs off the request/bind path on a background thread. This
    // parallelism trims the traversal/stat phase; it does not change time-to-
    // serving-ready. See docs/operations.md.
    builder.build_parallel().run(|| {
        Box::new(|entry| {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(error = %e, "Error walking directory");
                    return ignore::WalkState::Continue;
                }
            };

            let path = entry.path();

            // Detect project roots (directories whose `.git` is either a
            // directory — the main checkout — or a regular file — a
            // `git worktree`'s pointer to the shared `.git/worktrees/<name>/`
            // directory). Both kinds are valid project roots; without the
            // file-check, sibling worktrees of a repo are never discovered
            // and the worktree-aware analytics filter has nothing to scope
            // (only the main checkout would be indexed).
            if path.is_dir() {
                let dot_git = path.join(".git");
                if dot_git.is_dir() || dot_git.is_file() {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "unknown".into());

                    project_roots.insert(
                        path.to_path_buf(),
                        ProjectRoot {
                            workspace_path: workspace_path.to_string(),
                            name,
                        },
                    );

                    // Load project override if .pgmcp.toml exists
                    if let Some(override_config) = config::ProjectOverride::load(path) {
                        project_overrides.insert(path.to_path_buf(), override_config);
                    }

                    debug!(path = %path.display(), "Discovered project root");
                }
                return ignore::WalkState::Continue;
            }

            // Skip non-files
            if !path.is_file() {
                return ignore::WalkState::Continue;
            }

            // Check if this file type is configured
            if !is_configured_path(config, path) {
                return ignore::WalkState::Continue;
            }

            // Check exclude patterns
            let path_str = path.to_string_lossy();
            let excluded = config.indexer.exclude_patterns.iter().any(|pattern| {
                if let Some(suffix) = pattern.strip_prefix('*') {
                    path_str.ends_with(suffix)
                } else {
                    path_str.contains(pattern)
                }
            });

            if excluded {
                return ignore::WalkState::Continue;
            }

            // Submit for indexing
            if file_tx.send(path.to_path_buf()).is_err() {
                return ignore::WalkState::Quit; // Channel closed
            }
            ignore::WalkState::Continue
        })
    });
}

/// Scan `~/.claude/` with hardcoded noise excludes. Files are submitted directly
/// without `.gitignore` since `~/.claude/` has no `.git/`.
fn scan_claude_dir(
    claude_dir: &Path,
    workspace_path: &str,
    config: &Config,
    file_tx: &Sender<PathBuf>,
) {
    let mut builder = WalkBuilder::new(claude_dir);
    builder.hidden(false); // Allow all files (the dir itself is hidden)
    builder.git_ignore(false); // No .git/ in ~/.claude/
    builder.git_global(false);
    builder.git_exclude(false);

    let mut count: u64 = 0;

    for entry in builder.build() {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "Error walking ~/.claude/");
                continue;
            }
        };

        let path = entry.path();

        if path.is_dir() {
            continue;
        }

        if !path.is_file() {
            continue;
        }

        // Apply hardcoded Claude dir excludes
        let path_str = path.to_string_lossy();
        let excluded = CLAUDE_DIR_EXCLUDES.iter().any(|excl| {
            // Match as a path component or file name
            let relative = path.strip_prefix(claude_dir).unwrap_or(path);
            let rel_str = relative.to_string_lossy();
            // Match directory component or exact file name
            rel_str.starts_with(excl)
                || rel_str.starts_with(&format!("{}/", excl))
                || relative
                    .file_name()
                    .map(|f| f.to_string_lossy() == *excl)
                    .unwrap_or(false)
        });

        if excluded {
            continue;
        }

        // Check configured extensions
        if !is_configured_path(config, path) {
            continue;
        }

        // Apply global exclude patterns too
        let global_excluded = config.indexer.exclude_patterns.iter().any(|pattern| {
            if let Some(suffix) = pattern.strip_prefix('*') {
                path_str.ends_with(suffix)
            } else {
                path_str.contains(pattern)
            }
        });

        if global_excluded {
            continue;
        }

        if file_tx.send(path.to_path_buf()).is_err() {
            break;
        }
        count += 1;
    }

    info!(files = count, path = %workspace_path, "Scanned ~/.claude/ directory");
}

/// Scan `~/.codex/` with an allow-list. Unlike `~/.claude/`, Codex keeps
/// auth material, sqlite state, logs, shell snapshots, and plugin checkouts
/// directly under its home directory.
fn scan_codex_dir(
    codex_dir: &Path,
    workspace_path: &str,
    config: &Config,
    file_tx: &Sender<PathBuf>,
) {
    let mut builder = WalkBuilder::new(codex_dir);
    builder.hidden(false);
    builder.git_ignore(false);
    builder.git_global(false);
    builder.git_exclude(false);

    let mut count: u64 = 0;

    for entry in builder.build() {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "Error walking ~/.codex/");
                continue;
            }
        };

        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        if !should_index_codex_file(codex_dir, path, config) {
            continue;
        }

        if file_tx.send(path.to_path_buf()).is_err() {
            break;
        }
        count += 1;
    }

    info!(files = count, path = %workspace_path, "Scanned ~/.codex/ directory");
}

fn should_index_codex_file(codex_dir: &Path, path: &Path, config: &Config) -> bool {
    let relative = path.strip_prefix(codex_dir).unwrap_or(path);

    if !codex_relative_path_allowed(relative) {
        return false;
    }

    if !is_configured_path(config, path) {
        return false;
    }

    let path_str = path.to_string_lossy();
    !config.indexer.exclude_patterns.iter().any(|pattern| {
        if let Some(suffix) = pattern.strip_prefix('*') {
            path_str.ends_with(suffix)
        } else {
            path_str.contains(pattern)
        }
    })
}

/// Scan `~/Papers/` as a synthetic project named "Papers", applying
/// source-form deduplication so that an `invoice.org` + `invoice.tex` +
/// `invoice.pdf` triplet only indexes the highest-priority form.
fn scan_papers_dir(
    papers_dir: &Path,
    workspace_path: &str,
    config: &Config,
    file_tx: &Sender<PathBuf>,
    project_override: Option<&config::ProjectOverride>,
) {
    scan_synthetic_doc_dir(
        papers_dir,
        workspace_path,
        config,
        file_tx,
        PAPERS_DIR_EXCLUDES,
        project_override,
        "~/Papers/",
    );
}

/// Scan `~/Documents/` as a synthetic project named "Documents". Same
/// behavior as `scan_papers_dir` with the Documents-specific excludes.
fn scan_documents_dir(
    documents_dir: &Path,
    workspace_path: &str,
    config: &Config,
    file_tx: &Sender<PathBuf>,
    project_override: Option<&config::ProjectOverride>,
) {
    scan_synthetic_doc_dir(
        documents_dir,
        workspace_path,
        config,
        file_tx,
        DOCUMENTS_DIR_EXCLUDES,
        project_override,
        "~/Documents/",
    );
}

/// Shared synthetic-document-directory walker. Mirrors `scan_claude_dir`
/// but layers source-form deduplication on top of the candidate list
/// (group by `(parent_dir, file_stem)`; keep one entry per group per the
/// priority list) so that build outputs don't shadow their sources.
fn scan_synthetic_doc_dir(
    dir: &Path,
    workspace_path: &str,
    config: &Config,
    file_tx: &Sender<PathBuf>,
    extra_excludes: &[&str],
    project_override: Option<&config::ProjectOverride>,
    log_label: &str,
) {
    let mut builder = WalkBuilder::new(dir);
    builder.hidden(false);
    builder.git_ignore(false);
    builder.git_global(false);
    builder.git_exclude(false);

    let mut candidates: Vec<PathBuf> = Vec::new();

    for entry in builder.build() {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "Error walking {}", log_label);
                continue;
            }
        };

        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        // Synthetic-dir-specific excludes (LaTeX build artifacts, etc.).
        if matches_any_pattern(path, dir, extra_excludes) {
            continue;
        }

        if !is_configured_path(config, path) {
            continue;
        }

        // Global exclude_patterns.
        let path_str = path.to_string_lossy();
        let globally_excluded = config.indexer.exclude_patterns.iter().any(|pattern| {
            if let Some(suffix) = pattern.strip_prefix('*') {
                path_str.ends_with(suffix)
            } else {
                path_str.contains(pattern)
            }
        });
        if globally_excluded {
            continue;
        }

        candidates.push(path.to_path_buf());
    }

    // Resolve effective source priority: per-project override replaces
    // the global list (replace semantics, not OR — order matters).
    let effective_priority: Vec<String> = project_override
        .and_then(|o| o.indexer.as_ref())
        .and_then(|i| i.source_priority.clone())
        .unwrap_or_else(|| config.indexer.source_priority.clone());

    let pre_count = candidates.len();
    let keepers = dedup_source_forms(candidates, &effective_priority);
    let post_count = keepers.len();
    if pre_count != post_count {
        debug!(
            label = log_label,
            before = pre_count,
            after = post_count,
            removed = pre_count - post_count,
            "Source-form dedup collapsed sibling duplicates"
        );
    }

    let mut sent: u64 = 0;
    for path in keepers {
        if file_tx.send(path).is_err() {
            break;
        }
        sent += 1;
    }
    info!(files = sent, path = %workspace_path, label = %log_label, "Scanned synthetic doc directory");
}

/// True when any pattern in `patterns` matches `path`. Patterns starting
/// with `*` are suffix matches; otherwise treated as path-component
/// substring matches against the relative path under `root`.
fn matches_any_pattern(path: &Path, root: &Path, patterns: &[&str]) -> bool {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let rel_str = relative.to_string_lossy();
    patterns.iter().any(|pat| {
        if let Some(suffix) = pat.strip_prefix('*') {
            rel_str.ends_with(suffix)
        } else {
            rel_str.contains(pat)
                || relative
                    .file_name()
                    .map(|f| f.to_string_lossy().starts_with(pat))
                    .unwrap_or(false)
        }
    })
}

/// Group input paths by `(parent_dir, file_stem)`. For each group whose
/// members have priority-listed extensions, keep only the entry whose
/// extension appears earliest in `priority`. Files whose extensions are
/// not in `priority` are kept unconditionally (they aren't competing with
/// anything to be deduplicated against).
///
/// Properties:
/// - Idempotent: `dedup(dedup(xs)) == dedup(xs)`.
/// - Subset: every output appears in input.
/// - Order-preserving: keepers are emitted in their original input order.
/// - Cross-directory paths never merge.
pub(crate) fn dedup_source_forms(files: Vec<PathBuf>, priority: &[String]) -> Vec<PathBuf> {
    use std::collections::HashMap;

    if files.is_empty() {
        return files;
    }
    let prio_idx: HashMap<&str, usize> = priority
        .iter()
        .enumerate()
        .map(|(i, ext)| (ext.as_str(), i))
        .collect();

    // For each (parent, stem) group with priority members, remember the
    // input index of the winner. Non-priority files index into `keep` as
    // `true` unconditionally; priority files start `false` and the winner
    // gets flipped to `true`.
    let mut keep: Vec<bool> = vec![false; files.len()];
    let mut groups: HashMap<(PathBuf, String), (usize, usize)> = HashMap::new(); // (winner_idx, winner_prio)

    for (i, path) in files.iter().enumerate() {
        let parent = path.parent().map(Path::to_path_buf);
        let stem = path.file_stem().and_then(|s| s.to_str()).map(String::from);
        let ext = path.extension().and_then(|e| e.to_str());
        let (Some(parent), Some(stem)) = (parent, stem) else {
            keep[i] = true;
            continue;
        };
        let Some(ext) = ext else {
            keep[i] = true;
            continue;
        };
        match prio_idx.get(ext) {
            None => {
                // Extension not in priority list — not in any dedup
                // group; kept unconditionally.
                keep[i] = true;
            }
            Some(&prio) => {
                let key = (parent, stem);
                groups
                    .entry(key)
                    .and_modify(|(winner_idx, winner_prio)| {
                        if prio < *winner_prio {
                            *winner_idx = i;
                            *winner_prio = prio;
                        }
                    })
                    .or_insert((i, prio));
            }
        }
    }

    for (_, (winner_idx, _)) in groups {
        keep[winner_idx] = true;
    }

    files
        .into_iter()
        .zip(keep)
        .filter_map(|(p, k)| if k { Some(p) } else { None })
        .collect()
}

fn codex_relative_path_allowed(relative: &Path) -> bool {
    if relative == Path::new("config.toml") || relative == Path::new("history.jsonl") {
        return true;
    }

    match relative.components().next() {
        Some(std::path::Component::Normal(component)) if component.to_str() == Some("sessions") => {
            relative.extension().and_then(|e| e.to_str()) == Some("jsonl")
        }
        Some(std::path::Component::Normal(component)) if component.to_str() == Some("memories") => {
            true
        }
        _ => false,
    }
}

/// Find the project root for a given file path.
pub fn find_project_root<'a>(
    path: &Path,
    project_roots: &'a DashMap<PathBuf, ProjectRoot>,
) -> Option<(PathBuf, dashmap::mapref::one::Ref<'a, PathBuf, ProjectRoot>)> {
    let mut current = path.parent();
    while let Some(dir) = current {
        if let Some(root) = project_roots.get(&dir.to_path_buf()) {
            return Some((dir.to_path_buf(), root));
        }
        current = dir.parent();
    }
    None
}

/// Info about a discovered project root.
#[derive(Debug, Clone)]
pub struct ProjectRoot {
    pub workspace_path: String,
    pub name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_allowlist_includes_config_history_sessions_and_memories() {
        let config = Config::default();
        let codex_dir = Path::new("/home/user/.codex");

        for path in [
            "/home/user/.codex/config.toml",
            "/home/user/.codex/history.jsonl",
            "/home/user/.codex/sessions/2026/05/12/rollout.jsonl",
            "/home/user/.codex/memories/project.md",
        ] {
            assert!(
                should_index_codex_file(codex_dir, Path::new(path), &config),
                "expected Codex scanner to include {path}"
            );
        }
    }

    fn priority_default() -> Vec<String> {
        crate::config::DEFAULT_SOURCE_PRIORITY
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    }

    #[test]
    fn dedup_prefers_org_over_tex_over_pdf() {
        let inputs = vec![
            PathBuf::from("/p/invoice.pdf"),
            PathBuf::from("/p/invoice.tex"),
            PathBuf::from("/p/invoice.org"),
        ];
        let out = dedup_source_forms(inputs, &priority_default());
        assert_eq!(out, vec![PathBuf::from("/p/invoice.org")]);
    }

    #[test]
    fn dedup_prefers_tex_over_pdf_when_no_org() {
        let inputs = vec![PathBuf::from("/p/paper.pdf"), PathBuf::from("/p/paper.tex")];
        let out = dedup_source_forms(inputs, &priority_default());
        assert_eq!(out, vec![PathBuf::from("/p/paper.tex")]);
    }

    #[test]
    fn dedup_keeps_files_with_unknown_extensions() {
        // `.csv` isn't in the priority list, so it should be kept
        // unconditionally even when an `.org` sibling exists.
        let inputs = vec![PathBuf::from("/p/data.csv"), PathBuf::from("/p/data.org")];
        let mut out = dedup_source_forms(inputs, &priority_default());
        out.sort();
        assert_eq!(
            out,
            vec![PathBuf::from("/p/data.csv"), PathBuf::from("/p/data.org")]
        );
    }

    #[test]
    fn dedup_does_not_dedup_across_parent_dirs() {
        let inputs = vec![PathBuf::from("/a/paper.pdf"), PathBuf::from("/b/paper.pdf")];
        let out = dedup_source_forms(inputs.clone(), &priority_default());
        assert_eq!(out, inputs);
    }

    #[test]
    fn dedup_singleton_passes_through() {
        let inputs = vec![PathBuf::from("/p/only.pdf")];
        let out = dedup_source_forms(inputs.clone(), &priority_default());
        assert_eq!(out, inputs);
    }

    #[test]
    fn dedup_idempotent() {
        let inputs = vec![
            PathBuf::from("/p/invoice.pdf"),
            PathBuf::from("/p/invoice.tex"),
            PathBuf::from("/p/invoice.org"),
            PathBuf::from("/q/notes.md"),
            PathBuf::from("/q/notes.pdf"),
        ];
        let prio = priority_default();
        let once = dedup_source_forms(inputs, &prio);
        let twice = dedup_source_forms(once.clone(), &prio);
        assert_eq!(once, twice);
    }

    #[test]
    fn dedup_per_project_priority_replaces_global() {
        // Custom priority puts `pdf` before `org` — caller can prefer PDFs.
        let custom = vec!["pdf".to_string(), "org".to_string()];
        let inputs = vec![
            PathBuf::from("/p/invoice.pdf"),
            PathBuf::from("/p/invoice.org"),
        ];
        let out = dedup_source_forms(inputs, &custom);
        assert_eq!(out, vec![PathBuf::from("/p/invoice.pdf")]);
    }

    #[test]
    fn dedup_empty_priority_no_dedup() {
        let inputs = vec![
            PathBuf::from("/p/invoice.pdf"),
            PathBuf::from("/p/invoice.org"),
        ];
        let out = dedup_source_forms(inputs.clone(), &Vec::new());
        // Every extension is "unknown" in the empty priority — both kept.
        assert_eq!(out, inputs);
    }

    use proptest::prelude::*;
    proptest! {
        #[test]
        fn prop_dedup_subset(
            paths in prop::collection::vec(
                "[a-z]{1,4}/[a-z]{1,4}\\.(pdf|tex|org|md)",
                0..30,
            )
        ) {
            let inputs: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
            let out = dedup_source_forms(inputs.clone(), &priority_default());
            for p in &out {
                prop_assert!(inputs.contains(p));
            }
            prop_assert!(out.len() <= inputs.len());
        }

        #[test]
        fn prop_dedup_idempotent(
            paths in prop::collection::vec(
                "[a-z]{1,4}/[a-z]{1,4}\\.(pdf|tex|org|md|csv)",
                0..30,
            )
        ) {
            let inputs: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
            let prio = priority_default();
            let once = dedup_source_forms(inputs, &prio);
            let twice = dedup_source_forms(once.clone(), &prio);
            prop_assert_eq!(once, twice);
        }
    }

    #[test]
    fn codex_allowlist_excludes_secrets_state_cache_and_snapshots() {
        let config = Config::default();
        let codex_dir = Path::new("/home/user/.codex");

        for path in [
            "/home/user/.codex/auth.json",
            "/home/user/.codex/state_5.sqlite",
            "/home/user/.codex/log/codex-tui.log",
            "/home/user/.codex/cache/tool.json",
            "/home/user/.codex/tmp/plugins/README.md",
            "/home/user/.codex/shell_snapshots/snapshot.sh",
            "/home/user/.codex/sessions/2026/05/12/notes.md",
        ] {
            assert!(
                !should_index_codex_file(codex_dir, Path::new(path), &config),
                "expected Codex scanner to exclude {path}"
            );
        }
    }
}
