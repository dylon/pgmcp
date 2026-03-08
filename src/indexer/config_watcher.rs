//! Watch config.toml for changes and hot-reload into ArcSwap.
//!
//! Watches the **parent directory** of config.toml (non-recursive) because editors
//! often delete+recreate files on save. Debounces events (500ms) before reloading.
//! Compares old vs new workspace paths and sends WatcherCommands for dynamic
//! watch/unwatch/rescan. Logs warnings for cold config sections that require restart.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;
use crossbeam_channel::Sender;
use notify::{RecursiveMode, Watcher};
use tracing::{error, info, warn};

use crate::config::Config;
use crate::error::PgmcpError;
use crate::stats::tracker::StatsTracker;

/// Command sent from config watcher to the event processor.
pub enum WatcherCommand {
    /// Start watching a new workspace path.
    Watch(PathBuf),
    /// Stop watching a removed workspace path.
    Unwatch(PathBuf),
    /// Re-scan a workspace path for new/changed files.
    Rescan(PathBuf),
}

/// Handle to the config watcher. Dropping it stops watching.
pub struct ConfigWatcherHandle {
    _watcher: notify::RecommendedWatcher,
    _handler: std::thread::JoinHandle<()>,
}

/// Start watching config.toml for changes and hot-reload into ArcSwap.
///
/// Creates a `notify::RecommendedWatcher` on the parent directory of config.toml,
/// spawns a handler thread that debounces events, reloads config, sends watcher
/// commands for workspace path changes, and logs cold-change warnings.
pub fn start_config_watcher(
    config: Arc<ArcSwap<Config>>,
    config_path: PathBuf,
    watcher_cmd_tx: Sender<WatcherCommand>,
    shutdown: Arc<AtomicBool>,
    stats: Arc<StatsTracker>,
) -> Result<ConfigWatcherHandle, PgmcpError> {
    let (event_tx, event_rx) = crossbeam_channel::bounded::<PathBuf>(64);

    // Watch the parent directory (editors often delete+recreate files on save)
    let config_dir = config_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let config_filename = config_path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("config.toml"));

    let tx = event_tx;
    let filename_for_watcher = config_filename;
    let mut watcher = notify::recommended_watcher(
        move |res: Result<notify::Event, notify::Error>| {
            if let Ok(event) = res {
                match event.kind {
                    notify::EventKind::Create(_)
                    | notify::EventKind::Modify(_)
                    | notify::EventKind::Remove(_) => {
                        for path in &event.paths {
                            if path.file_name() == Some(&filename_for_watcher) {
                                let _ = tx.send(path.clone());
                            }
                        }
                    }
                    _ => {}
                }
            }
        },
    )?;

    // Create config dir if it doesn't exist
    if !config_dir.exists() {
        std::fs::create_dir_all(&config_dir)
            .map_err(|e| PgmcpError::file_io(&config_dir, e))?;
    }

    watcher.watch(&config_dir, RecursiveMode::NonRecursive)?;
    info!(path = %config_dir.display(), "Watching config directory for changes");

    let handler = std::thread::Builder::new()
        .name("pgmcp-config-watcher".into())
        .spawn(move || {
            config_watcher_loop(config, config_path, event_rx, watcher_cmd_tx, shutdown, stats);
        })
        .map_err(|e| PgmcpError::Other(format!("Failed to spawn config watcher thread: {}", e)))?;

    Ok(ConfigWatcherHandle {
        _watcher: watcher,
        _handler: handler,
    })
}

fn config_watcher_loop(
    config: Arc<ArcSwap<Config>>,
    config_path: PathBuf,
    event_rx: crossbeam_channel::Receiver<PathBuf>,
    watcher_cmd_tx: Sender<WatcherCommand>,
    shutdown: Arc<AtomicBool>,
    stats: Arc<StatsTracker>,
) {
    loop {
        if shutdown.load(Ordering::Acquire) {
            break;
        }

        match event_rx.recv_timeout(std::time::Duration::from_millis(500)) {
            Ok(_) => {
                // Debounce: wait 500ms and drain any additional events
                std::thread::sleep(std::time::Duration::from_millis(500));
                while event_rx.try_recv().is_ok() {}

                if shutdown.load(Ordering::Acquire) {
                    break;
                }

                // Reload config
                let old_config = config.load();
                match Config::load(Some(&config_path)) {
                    Ok(new_config) => {
                        // Compare workspace paths for watch/unwatch commands
                        let (added, removed) = diff_workspace_paths(
                            &old_config.workspace.paths,
                            &new_config.workspace.paths,
                        );

                        // Log cold section changes
                        log_cold_changes(&old_config, &new_config);

                        // Store new config
                        config.store(Arc::new(new_config));
                        stats.config_reloads.fetch_add(1, Ordering::Relaxed);
                        info!("Global configuration reloaded");

                        // Send watcher commands for workspace path changes
                        for path in removed {
                            let pb = PathBuf::from(&path);
                            let _ = watcher_cmd_tx.send(WatcherCommand::Unwatch(pb));
                        }
                        for path in added {
                            let pb = PathBuf::from(&path);
                            let _ = watcher_cmd_tx.send(WatcherCommand::Watch(pb.clone()));
                            let _ = watcher_cmd_tx.send(WatcherCommand::Rescan(pb));
                        }
                    }
                    Err(e) => {
                        error!(
                            error = %e,
                            path = %config_path.display(),
                            "Failed to reload config, keeping previous"
                        );
                        stats.config_reload_errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                // Normal timeout, check shutdown and loop
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                break;
            }
        }
    }
}

/// Compute added and removed workspace paths between old and new configs.
fn diff_workspace_paths(old: &[String], new: &[String]) -> (Vec<String>, Vec<String>) {
    let old_set: HashSet<&str> = old.iter().map(|s| s.as_str()).collect();
    let new_set: HashSet<&str> = new.iter().map(|s| s.as_str()).collect();

    let added: Vec<String> = new_set
        .difference(&old_set)
        .map(|s| s.to_string())
        .collect();
    let removed: Vec<String> = old_set
        .difference(&new_set)
        .map(|s| s.to_string())
        .collect();

    (added, removed)
}

/// Log warnings for cold config sections that require a restart to take effect.
fn log_cold_changes(old: &Config, new: &Config) {
    if old.database != new.database {
        warn!("database.* changed — restart required for changes to take effect");
    }
    if old.embeddings.model != new.embeddings.model
        || old.embeddings.pool_size != new.embeddings.pool_size
        || old.embeddings.dimensions != new.embeddings.dimensions
        || old.embeddings.use_gpu != new.embeddings.use_gpu
    {
        warn!("embeddings.model/pool_size/dimensions/use_gpu changed — restart required");
    }
    if old.mcp.host != new.mcp.host || old.mcp.port != new.mcp.port {
        warn!("mcp.host/port changed — restart required for changes to take effect");
    }
    if old.metrics != new.metrics {
        warn!("metrics.* changed — restart required for changes to take effect");
    }
    if old.logging != new.logging {
        warn!("logging.* changed — restart required for changes to take effect");
    }
    if old.work_pool != new.work_pool {
        warn!("work_pool.* changed — restart required for changes to take effect");
    }
    if old.cron != new.cron {
        warn!("cron.* changed — restart required for changes to take effect");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_diff_workspace_paths_empty() {
        let (added, removed) = diff_workspace_paths(&[], &[]);
        assert!(added.is_empty());
        assert!(removed.is_empty());
    }

    #[test]
    fn test_diff_workspace_paths_additions() {
        let old = vec![];
        let new = vec!["/home/user/project".to_string()];
        let (added, removed) = diff_workspace_paths(&old, &new);
        assert_eq!(added, vec!["/home/user/project"]);
        assert!(removed.is_empty());
    }

    #[test]
    fn test_diff_workspace_paths_removals() {
        let old = vec!["/home/user/project".to_string()];
        let new = vec![];
        let (added, removed) = diff_workspace_paths(&old, &new);
        assert!(added.is_empty());
        assert_eq!(removed, vec!["/home/user/project"]);
    }

    #[test]
    fn test_diff_workspace_paths_mixed() {
        let old = vec!["/a".to_string(), "/b".to_string()];
        let new = vec!["/b".to_string(), "/c".to_string()];
        let (added, removed) = diff_workspace_paths(&old, &new);
        assert_eq!(added, vec!["/c"]);
        assert_eq!(removed, vec!["/a"]);
    }

    #[test]
    fn test_diff_workspace_paths_no_change() {
        let old = vec!["/a".to_string(), "/b".to_string()];
        let new = vec!["/a".to_string(), "/b".to_string()];
        let (added, removed) = diff_workspace_paths(&old, &new);
        assert!(added.is_empty());
        assert!(removed.is_empty());
    }
}
