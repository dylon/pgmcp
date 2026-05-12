//! Full workspace scanner using ignore::WalkParallel.
//!
//! Discovers project roots via .git/ directory heuristic.
//! Respects .gitignore files.
//! Auto-discovers agent homes such as `~/.claude/` and `~/.codex/` as
//! synthetic projects.

use std::path::{Path, PathBuf};

use crossbeam_channel::Sender;
use dashmap::DashMap;
use ignore::WalkBuilder;
use tracing::{debug, info};

use crate::config::{self, Config};

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

/// Scan all configured workspace paths and submit files for indexing.
/// Also auto-discovers `~/.claude/` and `~/.codex/` if they exist.
pub fn scan_workspaces(
    config: &Config,
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

    // Auto-discover ~/.claude/ if it exists
    if let Some(claude_dir) = Config::claude_dir() {
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

        scan_claude_dir(&claude_dir, &claude_path_str, config, &file_tx);
    }

    // Auto-discover ~/.codex/ if it exists. Codex contains credentials,
    // sqlite state, caches, and shell snapshots, so this path is allow-listed
    // instead of broadly indexing every configured extension.
    if let Some(codex_dir) = Config::codex_dir() {
        let codex_path_str = codex_dir.to_string_lossy().into_owned();
        info!(path = %codex_path_str, "Auto-discovered ~/.codex/ directory");

        project_roots.insert(
            codex_dir.clone(),
            ProjectRoot {
                workspace_path: codex_path_str.clone(),
                name: "codex".into(),
            },
        );

        scan_codex_dir(&codex_dir, &codex_path_str, config, &file_tx);
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

    // Walk the directory tree
    for entry in builder.build() {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "Error walking directory");
                continue;
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
            continue;
        }

        // Skip non-files
        if !path.is_file() {
            continue;
        }

        // Check if this file type is configured
        if !config.indexer.is_configured_extension(path) {
            continue;
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
            continue;
        }

        // Submit for indexing
        if file_tx.send(path.to_path_buf()).is_err() {
            break; // Channel closed
        }
    }
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
        if !config.indexer.is_configured_extension(path) {
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

    if !config.indexer.is_configured_extension(path) {
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
