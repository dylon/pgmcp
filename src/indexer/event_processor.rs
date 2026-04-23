//! Event processor: wires reactive pipeline for file indexing.
//!
//! Watcher events -> filter -> debounce -> WorkPool dispatch
//! Scanner paths -> map -> WorkPool dispatch (low priority)
//!
//! Handles:
//! - `.pgmcp.toml` change detection → updates project override cache
//! - Per-project override application (file_types, exclude_patterns, max_file_size)
//! - WatcherCommand processing for dynamic workspace watch/unwatch/rescan

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use arc_swap::ArcSwap;
use crossbeam_channel::Sender;
use dashmap::DashMap;
use notify::{RecursiveMode, Watcher};
use tracing::{error, info, warn};

use crate::config::{self, Config};
use crate::daemon_state::{DaemonLifecycle, DaemonPhase};
use crate::embed::pool::EmbedIndexRequest;
use crate::indexer::{config_watcher::WatcherCommand, scanner, watcher};
use crate::reactive::operators;
use crate::shutdown::ShutdownCoordinator;
use crate::stats::tracker::StatsTracker;
use crate::work_pool::pool::{Priority, WorkPool};

/// Handle returned from start_indexing. Dropping it stops the file watcher.
#[allow(dead_code)]
pub struct IndexerHandle {
    _watcher: Arc<std::sync::Mutex<notify::RecommendedWatcher>>,
    _subscriptions: Vec<crate::reactive::subscription::Subscription>,
    project_roots: Arc<DashMap<PathBuf, scanner::ProjectRoot>>,
    _watcher_cmd_thread: Option<std::thread::JoinHandle<()>>,
}

#[allow(dead_code)]
impl IndexerHandle {
    pub fn project_roots(&self) -> Arc<DashMap<PathBuf, scanner::ProjectRoot>> {
        Arc::clone(&self.project_roots)
    }
}

/// Start the full indexing pipeline.
pub fn start_indexing(
    config: Arc<ArcSwap<Config>>,
    db_pool: sqlx::PgPool,
    work_pool: Arc<WorkPool>,
    embed_tx: Sender<EmbedIndexRequest>,
    stats: Arc<StatsTracker>,
    shutdown: ShutdownCoordinator,
    project_overrides: Arc<DashMap<PathBuf, config::ProjectOverride>>,
    watcher_cmd_rx: crossbeam_channel::Receiver<WatcherCommand>,
    lifecycle: DaemonLifecycle,
) -> Result<IndexerHandle, crate::error::PgmcpError> {
    let config_snapshot = config.load();
    let project_roots: Arc<DashMap<PathBuf, scanner::ProjectRoot>> = Arc::new(DashMap::new());

    // Capture the tokio runtime handle so WorkPool threads can run async code.
    // This must be called while we're on a tokio runtime thread (which we are,
    // since start_indexing is called from run_server inside #[tokio::main]).
    let rt_handle = tokio::runtime::Handle::current();

    // 1. Start file watcher
    let (event_tx, event_rx) = crossbeam_channel::bounded(4096);
    let raw_watcher = watcher::start_watching(
        &config_snapshot.workspace.paths,
        event_tx,
        Arc::clone(&stats),
    )?;
    let watcher_handle = Arc::new(std::sync::Mutex::new(raw_watcher));

    // 2. Set up reactive pipeline for watcher events
    let config_for_filter = Arc::clone(&config);
    let project_roots_for_filter = Arc::clone(&project_roots);
    let project_overrides_for_filter = Arc::clone(&project_overrides);
    let stats_for_filter = Arc::clone(&stats);
    let filtered_rx = {
        let rx = event_rx;
        // Filter to only configured extensions and non-excluded paths
        let (tx, filtered_rx) = crossbeam_channel::bounded(2048);

        let shutdown_flag = shutdown.terminating_flag();
        std::thread::Builder::new()
            .name("pgmcp-event-filter".into())
            .spawn(move || {
                for event in rx {
                    if shutdown_flag.load(Ordering::Acquire) {
                        break;
                    }

                    let cfg = config_for_filter.load();

                    // Detect .pgmcp.toml changes → update override cache
                    if event.path.file_name() == Some(std::ffi::OsStr::new(".pgmcp.toml"))
                        && let Some(project_root) = event.path.parent()
                    {
                        match event.kind {
                            watcher::FileEventKind::Create | watcher::FileEventKind::Modify => {
                                if let Some(ovr) = config::ProjectOverride::load(project_root) {
                                    project_overrides_for_filter
                                        .insert(project_root.to_path_buf(), ovr);
                                    info!(
                                        path = %project_root.display(),
                                        "Loaded project config override"
                                    );
                                }
                            }
                            watcher::FileEventKind::Remove => {
                                project_overrides_for_filter.remove(&project_root.to_path_buf());
                                info!(
                                    path = %project_root.display(),
                                    "Removed project config override"
                                );
                            }
                        }
                    }
                    // Fall through — still index the .pgmcp.toml as a regular file

                    // Look up project override for this file
                    let project_override =
                        scanner::find_project_root(&event.path, &project_roots_for_filter)
                            .and_then(|(root, _)| {
                                project_overrides_for_filter.get(&root).map(|r| r.clone())
                            });

                    // Extension check: global OR project-level file_types
                    if event.kind != watcher::FileEventKind::Remove {
                        let global_match = cfg.indexer.is_configured_extension(&event.path);
                        let project_match = project_override
                            .as_ref()
                            .and_then(|o| o.indexer.as_ref())
                            .and_then(|i| i.file_types.as_ref())
                            .map(|ft| {
                                event
                                    .path
                                    .extension()
                                    .and_then(|e| e.to_str())
                                    .map(|ext| ft.iter().any(|f| f.extension == ext))
                                    .unwrap_or(false)
                            })
                            .unwrap_or(false);

                        if !global_match && !project_match {
                            continue;
                        }
                    }

                    // Exclude check: global AND project-level patterns
                    let path_str = event.path.to_string_lossy();
                    let excluded = cfg
                        .indexer
                        .exclude_patterns
                        .iter()
                        .any(|p| check_pattern(p, &path_str))
                        || project_override
                            .as_ref()
                            .and_then(|o| o.indexer.as_ref())
                            .and_then(|i| i.exclude_patterns.as_ref())
                            .map(|patterns| patterns.iter().any(|p| check_pattern(p, &path_str)))
                            .unwrap_or(false);

                    if excluded {
                        continue;
                    }

                    if tx.send(event).is_err() {
                        break;
                    }
                    stats_for_filter
                        .watcher_events_filtered
                        .fetch_add(1, Ordering::Relaxed);
                }
            })
            .expect("Failed to spawn event filter thread");

        filtered_rx
    };

    // 3. Debounce by path
    let debounce_ms = config_snapshot.indexer.debounce_ms;
    let debounced_rx = operators::debounce_by_key(
        filtered_rx,
        Duration::from_millis(debounce_ms),
        |event: &watcher::FileEvent| event.path.clone(),
    );

    // 4. Subscribe to debounced events and dispatch to work pool
    let work_pool_for_events = Arc::clone(&work_pool);
    let config_for_events = Arc::clone(&config);
    let db_for_events = db_pool.clone();
    let embed_tx_for_events = embed_tx.clone();
    let stats_for_events = Arc::clone(&stats);
    let stats_for_debounce = Arc::clone(&stats);
    let project_roots_for_events = Arc::clone(&project_roots);
    let project_overrides_for_events = Arc::clone(&project_overrides);

    let rt_for_events = rt_handle.clone();
    let event_sub = crate::reactive::observable::Observable::from_receiver(debounced_rx).subscribe(
        move |event: watcher::FileEvent| {
            stats_for_debounce
                .watcher_events_debounced
                .fetch_add(1, Ordering::Relaxed);

            let path = event.path.clone();
            let config = Arc::clone(&config_for_events);
            let db = db_for_events.clone();
            let embed_tx = embed_tx_for_events.clone();
            let stats = Arc::clone(&stats_for_events);
            let roots = Arc::clone(&project_roots_for_events);
            let overrides = Arc::clone(&project_overrides_for_events);
            let rt = rt_for_events.clone();

            work_pool_for_events.submit(
                move || {
                    rt.block_on(async {
                        handle_file_event(
                            &path,
                            &event.kind,
                            &config.load(),
                            &db,
                            &embed_tx,
                            &stats,
                            &roots,
                            &overrides,
                        )
                        .await;
                    });
                },
                Priority::High,
            );
        },
    );

    // 5. Start initial scan in background
    let config_for_scan = Arc::clone(&config);
    let work_pool_for_scan = Arc::clone(&work_pool);
    let db_for_scan = db_pool.clone();
    let embed_tx_for_scan = embed_tx.clone();
    let stats_for_scan = Arc::clone(&stats);
    let project_roots_for_scan = Arc::clone(&project_roots);
    let project_overrides_for_scan = Arc::clone(&project_overrides);
    let rt_for_scan = rt_handle.clone();

    let lifecycle_for_scan = lifecycle;
    std::thread::Builder::new()
        .name("pgmcp-scanner".into())
        .spawn(move || {
            let (file_tx, file_rx) = crossbeam_channel::bounded(4096);
            let config_snapshot = config_for_scan.load();

            // Load indexed file metadata for scan optimization (Level 1 skip)
            let metadata_map: std::collections::HashMap<
                String,
                crate::db::queries::IndexedFileMeta,
            > = match rt_for_scan.block_on(crate::db::queries::get_all_file_metadata(&db_for_scan))
            {
                Ok(metas) => {
                    let len = metas.len();
                    let mut map = std::collections::HashMap::with_capacity(len);
                    for meta in metas {
                        map.insert(meta.path.clone(), meta);
                    }
                    info!(
                        indexed_files = len,
                        "Loaded file metadata for scan optimization"
                    );
                    map
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "Failed to load file metadata, falling back to full scan"
                    );
                    std::collections::HashMap::new()
                }
            };

            // Walk directories in parallel
            let scan_config = config_snapshot.clone();
            let scan_roots = Arc::clone(&project_roots_for_scan);
            let scan_overrides = Arc::clone(&project_overrides_for_scan);
            let scan_handle = std::thread::Builder::new()
                .name("pgmcp-scan-walk".into())
                .spawn(move || {
                    scanner::scan_workspaces(&scan_config, file_tx, &scan_roots, &scan_overrides);
                })
                .expect("Failed to spawn scan walk thread");

            // Process discovered files with metadata-based filtering
            let mut total_scanned: u64 = 0;
            let mut skipped: u64 = 0;
            let mut submitted: u64 = 0;
            let mut seen_paths: std::collections::HashSet<String> =
                std::collections::HashSet::with_capacity(metadata_map.len());

            for path in file_rx {
                total_scanned += 1;
                seen_paths.insert(path.to_string_lossy().into_owned());

                // Level 1: metadata-based skip check (stat only, no file read)
                if let Some(db_meta) = metadata_map.get(&*path.to_string_lossy())
                    && let Ok(fs_meta) = std::fs::metadata(&path)
                {
                    let fs_size = fs_meta.len() as i64;
                    let fs_mtime: chrono::DateTime<chrono::Utc> = fs_meta
                        .modified()
                        .map(Into::into)
                        .unwrap_or_else(|_| chrono::Utc::now());

                    if fs_size == db_meta.size_bytes && fs_mtime <= db_meta.modified_at {
                        skipped += 1;
                        continue;
                    }
                }

                submitted += 1;

                let config = Arc::clone(&config_for_scan);
                let db = db_for_scan.clone();
                let embed_tx = embed_tx_for_scan.clone();
                let stats = Arc::clone(&stats_for_scan);
                let roots = Arc::clone(&project_roots_for_scan);
                let overrides = Arc::clone(&project_overrides_for_scan);
                let rt = rt_for_scan.clone();

                work_pool_for_scan.submit(
                    move || {
                        rt.block_on(async {
                            handle_file_event(
                                &path,
                                &watcher::FileEventKind::Create,
                                &config.load(),
                                &db,
                                &embed_tx,
                                &stats,
                                &roots,
                                &overrides,
                            )
                            .await;
                        });
                    },
                    Priority::Low,
                );
            }

            let _ = scan_handle.join();

            // Remove stale files: indexed in DB but no longer found on disk
            let stale_paths: Vec<String> = metadata_map
                .keys()
                .filter(|path| !seen_paths.contains(*path))
                .cloned()
                .collect();
            let stale_count = stale_paths.len() as u64;
            if !stale_paths.is_empty() {
                match rt_for_scan.block_on(crate::db::queries::delete_files_batch(
                    &db_for_scan,
                    &stale_paths,
                )) {
                    Ok(deleted) => {
                        info!(
                            detected = stale_count,
                            deleted, "Removed stale files from index"
                        );
                    }
                    Err(e) => {
                        error!(count = stale_count, error = %e, "Failed to remove stale files");
                    }
                }
                stats_for_scan
                    .files_stale_removed
                    .fetch_add(stale_count, Ordering::Relaxed);

                // Clean up projects left empty after stale file removal
                match rt_for_scan
                    .block_on(crate::db::queries::cleanup_orphaned_projects(&db_for_scan))
                {
                    Ok(deleted) if deleted > 0 => {
                        info!(
                            projects_removed = deleted,
                            "Cleaned up orphaned projects after stale file removal"
                        );
                    }
                    Ok(_) => {}
                    Err(e) => {
                        error!(error = %e, "Failed to clean up orphaned projects");
                    }
                }
            }

            stats_for_scan
                .files_scanned
                .fetch_add(total_scanned, Ordering::Relaxed);
            stats_for_scan
                .files_skipped
                .fetch_add(skipped, Ordering::Relaxed);

            info!(
                total = total_scanned,
                unchanged = skipped,
                submitted,
                stale_removed = stale_count,
                "Initial scan complete"
            );

            // Signal that the daemon is ready for full operation
            lifecycle_for_scan.transition(DaemonPhase::Ready);
        })
        .expect("Failed to spawn scanner thread");

    // 6. Spawn watcher command handler thread
    let watcher_for_cmd = Arc::clone(&watcher_handle);
    let config_for_cmd = Arc::clone(&config);
    let work_pool_for_cmd = Arc::clone(&work_pool);
    let db_for_cmd = db_pool;
    let embed_tx_for_cmd = embed_tx;
    let stats_for_cmd = Arc::clone(&stats);
    let roots_for_cmd = Arc::clone(&project_roots);
    let overrides_for_cmd = Arc::clone(&project_overrides);
    let rt_for_cmd = rt_handle;
    let shutdown_for_cmd = shutdown.terminating_flag();

    let watcher_cmd_thread = std::thread::Builder::new()
        .name("pgmcp-watcher-cmd".into())
        .spawn(move || {
            for cmd in watcher_cmd_rx {
                if shutdown_for_cmd.load(Ordering::Acquire) {
                    break;
                }
                match cmd {
                    WatcherCommand::Watch(path) => {
                        if let Ok(mut w) = watcher_for_cmd.lock() {
                            match w.watch(&path, RecursiveMode::Recursive) {
                                Ok(()) => {
                                    info!(path = %path.display(), "Now watching new workspace");
                                }
                                Err(e) => {
                                    error!(
                                        path = %path.display(),
                                        error = %e,
                                        "Failed to watch new workspace"
                                    );
                                }
                            }
                        }
                    }
                    WatcherCommand::Unwatch(path) => {
                        if let Ok(mut w) = watcher_for_cmd.lock() {
                            match w.unwatch(&path) {
                                Ok(()) => {
                                    info!(
                                        path = %path.display(),
                                        "Stopped watching workspace"
                                    );
                                }
                                Err(e) => {
                                    warn!(
                                        path = %path.display(),
                                        error = %e,
                                        "Failed to unwatch workspace"
                                    );
                                }
                            }
                        }
                        // Clean up DB: delete all projects under this workspace path
                        let ws = path.to_string_lossy().to_string();
                        match rt_for_cmd.block_on(
                            crate::db::queries::delete_projects_by_workspace(&db_for_cmd, &ws)
                        ) {
                            Ok(deleted) if deleted > 0 => {
                                info!(
                                    workspace = %path.display(),
                                    projects_removed = deleted,
                                    "Cleaned up projects for removed workspace (cascaded to files, chunks, commits)"
                                );
                            }
                            Ok(_) => {}
                            Err(e) => {
                                error!(
                                    workspace = %path.display(),
                                    error = %e,
                                    "Failed to clean up projects for removed workspace"
                                );
                            }
                        }
                    }
                    WatcherCommand::Rescan(path) => {
                        rescan_workspace(
                            &path,
                            &config_for_cmd,
                            &work_pool_for_cmd,
                            &db_for_cmd,
                            &embed_tx_for_cmd,
                            &stats_for_cmd,
                            &roots_for_cmd,
                            &overrides_for_cmd,
                            &rt_for_cmd,
                        );
                    }
                }
            }
        })
        .expect("Failed to spawn watcher command thread");

    Ok(IndexerHandle {
        _watcher: watcher_handle,
        _subscriptions: vec![event_sub],
        project_roots,
        _watcher_cmd_thread: Some(watcher_cmd_thread),
    })
}

/// Check if a glob-like pattern matches a path string.
fn check_pattern(pattern: &str, path_str: &str) -> bool {
    if let Some(suffix) = pattern.strip_prefix('*') {
        path_str.ends_with(suffix)
    } else {
        path_str.contains(pattern)
    }
}

/// Re-scan a single workspace path and submit discovered files for indexing.
/// Called when config.toml gains a new workspace path.
fn rescan_workspace(
    workspace_path: &Path,
    config: &Arc<ArcSwap<Config>>,
    work_pool: &Arc<WorkPool>,
    db_pool: &sqlx::PgPool,
    embed_tx: &Sender<EmbedIndexRequest>,
    stats: &Arc<StatsTracker>,
    project_roots: &Arc<DashMap<PathBuf, scanner::ProjectRoot>>,
    project_overrides: &Arc<DashMap<PathBuf, config::ProjectOverride>>,
    rt_handle: &tokio::runtime::Handle,
) {
    let workspace_path_str = workspace_path.to_string_lossy().into_owned();
    info!(path = %workspace_path_str, "Re-scanning workspace");

    let (file_tx, file_rx) = crossbeam_channel::bounded(4096);
    let config_snapshot = config.load().clone();
    let walk_path = workspace_path.to_path_buf();
    let walk_path_str = workspace_path_str.clone();
    let walk_roots = Arc::clone(project_roots);
    let walk_overrides = Arc::clone(project_overrides);

    let walk_handle = std::thread::Builder::new()
        .name("pgmcp-rescan-walk".into())
        .spawn(move || {
            scanner::scan_single_workspace(
                &walk_path,
                &walk_path_str,
                &config_snapshot,
                &file_tx,
                &walk_roots,
                &walk_overrides,
            );
        })
        .expect("Failed to spawn rescan walk thread");

    // Process discovered files — no Level-1 metadata skip for new workspaces;
    // process_file does Level-2 content hash skip.
    let mut count = 0u64;
    for path in file_rx {
        count += 1;
        let config = Arc::clone(config);
        let db = db_pool.clone();
        let embed_tx = embed_tx.clone();
        let stats = Arc::clone(stats);
        let roots = Arc::clone(project_roots);
        let overrides = Arc::clone(project_overrides);
        let rt = rt_handle.clone();

        work_pool.submit(
            move || {
                rt.block_on(async {
                    handle_file_event(
                        &path,
                        &watcher::FileEventKind::Create,
                        &config.load(),
                        &db,
                        &embed_tx,
                        &stats,
                        &roots,
                        &overrides,
                    )
                    .await;
                });
            },
            Priority::Low,
        );
    }

    let _ = walk_handle.join();
    info!(path = %workspace_path_str, files = count, "Re-scan complete");
}

pub(crate) async fn handle_file_event(
    path: &Path,
    kind: &watcher::FileEventKind,
    config: &Config,
    db_pool: &sqlx::PgPool,
    embed_tx: &Sender<EmbedIndexRequest>,
    stats: &StatsTracker,
    project_roots: &DashMap<PathBuf, scanner::ProjectRoot>,
    project_overrides: &DashMap<PathBuf, config::ProjectOverride>,
) {
    match kind {
        watcher::FileEventKind::Remove => {
            let path_str = path.to_string_lossy();
            if let Err(e) = crate::db::queries::delete_file(db_pool, &path_str).await {
                error!(path = %path_str, error = %e, "Failed to delete file from index");
            }
        }
        watcher::FileEventKind::Create | watcher::FileEventKind::Modify => {
            // Find project root
            let (project_id, workspace_path, project_root_path) =
                match scanner::find_project_root(path, project_roots) {
                    Some((root_path, root_info)) => {
                        let root = root_info.clone();
                        drop(root_info); // Release DashMap ref

                        match crate::db::queries::upsert_project(
                            db_pool,
                            &root.workspace_path,
                            &root_path.to_string_lossy(),
                            &root.name,
                        )
                        .await
                        {
                            Ok(id) => (
                                id,
                                root_path.to_string_lossy().into_owned(),
                                Some(root_path),
                            ),
                            Err(e) => {
                                error!(error = %e, "Failed to upsert project");
                                return;
                            }
                        }
                    }
                    None => {
                        // No project root found, use the workspace path
                        let workspace = config.workspace.paths.first().cloned().unwrap_or_default();
                        match crate::db::queries::upsert_project(
                            db_pool, &workspace, &workspace, "default",
                        )
                        .await
                        {
                            Ok(id) => (id, workspace, None),
                            Err(e) => {
                                error!(error = %e, "Failed to upsert default project");
                                return;
                            }
                        }
                    }
                };

            // Look up per-project max_file_size_bytes override
            let max_file_size_override = project_root_path.as_ref().and_then(|root| {
                project_overrides
                    .get(root)
                    .and_then(|ovr| ovr.indexer.as_ref().and_then(|idx| idx.max_file_size_bytes))
            });

            if let Err(e) = super::processor::process_file(
                path,
                project_id,
                &workspace_path,
                config,
                db_pool,
                embed_tx,
                stats,
                max_file_size_override,
            )
            .await
            {
                let path_str = path.to_string_lossy();
                error!(path = %path_str, error = %e, "Failed to process file");
                stats.files_failed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}
