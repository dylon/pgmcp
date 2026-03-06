//! Full workspace scanner using ignore::WalkParallel.
//!
//! Discovers project roots via .git/ directory heuristic.
//! Respects .gitignore files.

use std::path::{Path, PathBuf};

use crossbeam_channel::Sender;
use dashmap::DashMap;
use ignore::WalkBuilder;
use tracing::{debug, info};

use crate::config::Config;

/// Scan all configured workspace paths and submit files for indexing.
pub fn scan_workspaces(
    config: &Config,
    file_tx: Sender<PathBuf>,
    project_roots: &DashMap<PathBuf, ProjectRoot>,
) {
    for workspace_path in &config.workspace.paths {
        let workspace = Path::new(workspace_path);
        if !workspace.exists() {
            tracing::warn!(path = %workspace_path, "Workspace path does not exist");
            continue;
        }

        info!(path = %workspace_path, "Scanning workspace");

        let mut builder = WalkBuilder::new(workspace);
        builder.hidden(true); // Skip hidden files (but .gitignore is still read)
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

            // Detect project roots (directories with .git/)
            if path.is_dir() {
                if path.join(".git").is_dir() {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "unknown".into());

                    project_roots.insert(
                        path.to_path_buf(),
                        ProjectRoot {
                            workspace_path: workspace_path.clone(),
                            name,
                        },
                    );
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
                if pattern.starts_with('*') {
                    path_str.ends_with(&pattern[1..])
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
