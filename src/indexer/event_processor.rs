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
use crate::context::SystemContext;
use crate::daemon_state::{DaemonLifecycle, DaemonPhase};
use crate::embed::pool::EmbedIndexRequest;
use crate::indexer::{config_watcher::WatcherCommand, scanner, watcher};
use crate::reactive::operators;
use crate::shutdown::ShutdownCoordinator;
use crate::stats::tracker::StatsTracker;

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
///
/// Replaces the previous 9-parameter signature. The bundled subsystems
/// (db, stats, config) come in via `ctx`; the indexer-specific deps
/// (work_pool, embed_tx, shutdown, project_overrides, watcher_cmd_rx,
/// lifecycle) stay separate because they aren't used by tools or the
/// REST API and don't belong in the shared `SystemContext`.
#[allow(clippy::too_many_arguments)]
pub fn start_indexing(
    ctx: SystemContext,
    embed_tx: Sender<EmbedIndexRequest>,
    shutdown: ShutdownCoordinator,
    project_overrides: Arc<DashMap<PathBuf, config::ProjectOverride>>,
    watcher_cmd_rx: crossbeam_channel::Receiver<WatcherCommand>,
    watcher_cmd_tx: crossbeam_channel::Sender<WatcherCommand>,
    lifecycle: DaemonLifecycle,
) -> Result<IndexerHandle, crate::error::PgmcpError> {
    let config = Arc::clone(ctx.config());
    let db = Arc::clone(ctx.db());
    let stats = Arc::clone(ctx.stats());
    let config_snapshot = config.load();
    let project_roots: Arc<DashMap<PathBuf, scanner::ProjectRoot>> = Arc::new(DashMap::new());

    // Capture the tokio runtime handle so WorkPool threads can run async code.
    // This must be called while we're on a tokio runtime thread (which we are,
    // since start_indexing is called from run_server inside #[tokio::main]).
    let rt_handle = tokio::runtime::Handle::current();

    // 1. Start file watcher.
    //
    // The watch set is the union of `config.workspace.paths` and any
    // synthetic roots that exist on disk (`~/.claude`, `~/.codex`,
    // `~/Papers`, `~/Documents`). Without including synthetic roots
    // here, edits to those directories drift until daemon restart —
    // the initial scan picks them up once, but no live inotify
    // events fire afterwards. See `effective_workspace_paths` in
    // `src/indexer/scanner.rs`.
    let watch_synthetic_roots = scanner::SyntheticRoots::from_home();
    let watch_paths = scanner::effective_workspace_paths(&config_snapshot, &watch_synthetic_roots);
    let (event_tx, event_rx) = crossbeam_channel::bounded(4096);
    let event_tx_for_reinit = event_tx.clone();
    let raw_watcher = watcher::start_watching(
        &watch_paths,
        event_tx,
        Arc::clone(&stats),
        Some(watcher_cmd_tx.clone()),
    )?;
    let watcher_handle = Arc::new(std::sync::Mutex::new(raw_watcher));

    // 2. Set up reactive pipeline for watcher events
    let config_for_filter = Arc::clone(&config);
    let project_roots_for_filter = Arc::clone(&project_roots);
    let project_overrides_for_filter = Arc::clone(&project_overrides);
    let stats_for_filter = Arc::clone(&stats);
    let phonetics_for_filter = Arc::clone(ctx.phonetics_registry());
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
                                    // P14.4 — install / reload the per-project
                                    // PgmcpPhonetics watcher when the override
                                    // declares a `[phonetics] rules_path`.
                                    if let Some(phon_ovr) = ovr.phonetics.as_ref()
                                        && let Some(rules_path) = phon_ovr.rules_path.as_ref()
                                    {
                                        let lang = phon_ovr.language.as_deref();
                                        if let Err(e) =
                                            crate::fuzzy::phonetic::install_phonetics_for_project(
                                                project_root,
                                                rules_path,
                                                lang,
                                                &phonetics_for_filter,
                                            )
                                        {
                                            warn!(
                                                path = %project_root.display(),
                                                rules_path = %rules_path.display(),
                                                error = %e,
                                                "P14.4: per-project PgmcpPhonetics install failed"
                                            );
                                        } else {
                                            info!(
                                                path = %project_root.display(),
                                                rules_path = %rules_path.display(),
                                                "P14.4: per-project PgmcpPhonetics installed"
                                            );
                                        }
                                    }
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
                                if phonetics_for_filter
                                    .remove(&project_root.to_path_buf())
                                    .is_some()
                                {
                                    info!(
                                        path = %project_root.display(),
                                        "P14.4: per-project PgmcpPhonetics removed (Drop tears down watcher)"
                                    );
                                }
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

    // 4. Subscribe to debounced events and submit IndexFile tasks to the
    //    inference pool (which now owns the entire file-indexing pipeline).
    let config_for_events = Arc::clone(&config);
    let db_for_events = Arc::clone(&db);
    let embed_tx_for_events = embed_tx.clone();
    let stats_for_debounce = Arc::clone(&stats);
    let project_roots_for_events = Arc::clone(&project_roots);
    let project_overrides_for_events = Arc::clone(&project_overrides);

    let event_sub = crate::reactive::observable::Observable::from_receiver(debounced_rx).subscribe(
        move |event: watcher::FileEvent| {
            stats_for_debounce
                .watcher_events_debounced
                .fetch_add(1, Ordering::Relaxed);

            let task = crate::embed::pool::IndexFileTask {
                path: event.path.clone(),
                kind: event.kind,
                config: Arc::clone(&config_for_events),
                db: Arc::clone(&db_for_events),
                project_roots: Arc::clone(&project_roots_for_events),
                project_overrides: Arc::clone(&project_overrides_for_events),
            };
            stats_for_debounce
                .files_submitted
                .fetch_add(1, Ordering::Relaxed);
            if let Err(e) = embed_tx_for_events.send(EmbedIndexRequest::IndexFile(task)) {
                error!(path = %event.path.display(), error = %e,
                       "Failed to submit IndexFile task");
            }
        },
    );

    // 5. Start initial scan in background
    let config_for_scan = Arc::clone(&config);
    let db_for_scan = Arc::clone(&db);
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
            > = match rt_for_scan.block_on(db_for_scan.get_all_file_metadata()) {
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
            let synthetic_roots = scanner::SyntheticRoots::from_home();
            let synthetic_roots_for_walk = synthetic_roots.clone();
            let scan_handle = std::thread::Builder::new()
                .name("pgmcp-scan-walk".into())
                .spawn(move || {
                    scanner::scan_workspaces(
                        &scan_config,
                        &synthetic_roots_for_walk,
                        file_tx,
                        &scan_roots,
                        &scan_overrides,
                    );
                })
                .expect("Failed to spawn scan walk thread");

            // Process discovered files with metadata-based filtering
            let mut total_scanned: u64 = 0;
            let mut skipped: u64 = 0;
            let mut submitted: u64 = 0;
            let mut seen_paths: std::collections::HashSet<String> =
                std::collections::HashSet::with_capacity(metadata_map.len());

            for path in file_rx {
                // Bail out early on SIGTERM so we don't enqueue more
                // work into `embed_tx_for_scan` only to have the next
                // send() fail when shutdown drops the receiver. See
                // plan ~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md
                // F3.
                if lifecycle_for_scan.is_stopping() {
                    break;
                }
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

                let task = crate::embed::pool::IndexFileTask {
                    path,
                    kind: watcher::FileEventKind::Create,
                    config: Arc::clone(&config_for_scan),
                    db: Arc::clone(&db_for_scan),
                    project_roots: Arc::clone(&project_roots_for_scan),
                    project_overrides: Arc::clone(&project_overrides_for_scan),
                };
                stats_for_scan
                    .files_submitted
                    .fetch_add(1, Ordering::Relaxed);
                if let Err(e) = embed_tx_for_scan.send(EmbedIndexRequest::IndexFile(task)) {
                    if lifecycle_for_scan.is_stopping() {
                        tracing::debug!("initial-scan channel closed during shutdown — exiting");
                    } else {
                        error!(
                            error = %e,
                            "Failed to submit IndexFile task during initial scan"
                        );
                    }
                    break;
                }
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
                match rt_for_scan.block_on(db_for_scan.delete_files_batch(&stale_paths)) {
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
                match rt_for_scan.block_on(db_for_scan.cleanup_orphaned_projects()) {
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

            // Mark every project under each scanned workspace as freshly
            // scanned. The per-file `upsert_project` path bumps
            // `last_scanned_at` whenever a file is processed, but a
            // workspace whose files are all unchanged would never trigger
            // an upsert and the column would never advance — defeating
            // the freshness signal external tools rely on. This bulk
            // UPDATE catches that case in one cheap query per workspace.
            //
            // Synthetic-root projects share `workspace_path` with their
            // resolved canonical path (e.g. `/home/dylon/.claude`), so a
            // single UPDATE keyed on that string covers them too.
            let mut workspace_paths: Vec<String> = config_snapshot.workspace.paths.clone();
            if let Some(p) = synthetic_roots.claude.as_ref() {
                workspace_paths.push(p.to_string_lossy().into_owned());
            }
            if let Some(p) = synthetic_roots.codex.as_ref() {
                workspace_paths.push(p.to_string_lossy().into_owned());
            }
            if let Some(p) = synthetic_roots.papers.as_ref() {
                workspace_paths.push(p.to_string_lossy().into_owned());
            }
            if let Some(p) = synthetic_roots.documents.as_ref() {
                workspace_paths.push(p.to_string_lossy().into_owned());
            }
            for ws in &workspace_paths {
                match rt_for_scan.block_on(db_for_scan.update_projects_scanned_by_workspace(ws)) {
                    Ok(rows) => {
                        stats_for_scan
                            .last_scanned_writes
                            .fetch_add(rows, Ordering::Relaxed);
                    }
                    Err(e) => {
                        tracing::warn!(
                            workspace = %ws,
                            error = %e,
                            "Failed to update last_scanned_at after initial scan"
                        );
                    }
                }
            }

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
    let db_for_cmd = db;
    let embed_tx_for_cmd = embed_tx;
    let roots_for_cmd = Arc::clone(&project_roots);
    let overrides_for_cmd = Arc::clone(&project_overrides);
    let rt_for_cmd = rt_handle;
    let shutdown_for_cmd = shutdown.terminating_flag();
    let stats_for_cmd = Arc::clone(&stats);
    // Reinit-arm bindings — passed into the WatcherCommand::Reinit
    // handler so it can rebuild the watcher with the same plumbing as
    // the original `start_watching` call.
    let stats_for_reinit = Arc::clone(&stats);
    let reinit_cmd_tx = watcher_cmd_tx.clone();

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
                        match rt_for_cmd
                            .block_on(db_for_cmd.delete_projects_by_workspace(&ws))
                        {
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
                            &db_for_cmd,
                            &embed_tx_for_cmd,
                            &roots_for_cmd,
                            &overrides_for_cmd,
                            &rt_for_cmd,
                            &stats_for_cmd,
                        );
                    }
                    WatcherCommand::Reinit(paths) => {
                        // Inotify queue overflowed (or another watcher
                        // failure mode that requires a fresh handle).
                        // Build a new watcher with the same workspaces,
                        // swap it into the Mutex, and enqueue a Rescan
                        // per workspace so the index catches whatever
                        // events were dropped before the rearm.
                        let workspace_strs: Vec<String> =
                            paths.iter().map(|p| p.to_string_lossy().into_owned()).collect();
                        match watcher::start_watching(
                            &workspace_strs,
                            event_tx_for_reinit.clone(),
                            Arc::clone(&stats_for_reinit),
                            Some(reinit_cmd_tx.clone()),
                        ) {
                            Ok(new_watcher) => {
                                match watcher_for_cmd.lock() {
                                    Ok(mut w) => {
                                        *w = new_watcher;
                                        info!(
                                            workspaces = paths.len(),
                                            "watcher re-armed after inotify overflow"
                                        );
                                    }
                                    Err(poisoned) => {
                                        // Mutex poisoned by a panicked
                                        // command handler — recover by
                                        // overwriting with the fresh
                                        // watcher anyway, since the
                                        // poison data is now obsolete.
                                        let mut w = poisoned.into_inner();
                                        *w = new_watcher;
                                        warn!(
                                            workspaces = paths.len(),
                                            "watcher re-armed via poisoned mutex recovery"
                                        );
                                    }
                                }
                                for p in paths {
                                    let _ = reinit_cmd_tx.send(WatcherCommand::Rescan(p));
                                }
                            }
                            Err(e) => {
                                error!(
                                    error = %e,
                                    "watcher re-arm failed; events will continue to be lost \
                                     until daemon restart"
                                );
                            }
                        }
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
#[allow(clippy::too_many_arguments)]
fn rescan_workspace(
    workspace_path: &Path,
    config: &Arc<ArcSwap<Config>>,
    db: &Arc<dyn crate::db::DbClient>,
    embed_tx: &Sender<EmbedIndexRequest>,
    project_roots: &Arc<DashMap<PathBuf, scanner::ProjectRoot>>,
    project_overrides: &Arc<DashMap<PathBuf, config::ProjectOverride>>,
    rt_handle: &tokio::runtime::Handle,
    stats: &Arc<StatsTracker>,
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

    // Load indexed file metadata for Level-1 (size+mtime) skip. Mirrors the
    // initial-scan path. Without this, every path discovered by the walker is
    // unconditionally read end-to-end via process_file just to compute a
    // content hash that almost always matches what's already in the DB —
    // burning I/O, malloc churn, and embed-channel backpressure on tens of
    // thousands of files that haven't changed.
    let metadata_map: std::collections::HashMap<String, crate::db::queries::IndexedFileMeta> =
        match rt_handle.block_on(db.get_all_file_metadata()) {
            Ok(metas) => {
                let mut map = std::collections::HashMap::with_capacity(metas.len());
                for meta in metas {
                    map.insert(meta.path.clone(), meta);
                }
                map
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Failed to load file metadata for rescan, falling back to full re-read"
                );
                std::collections::HashMap::new()
            }
        };

    let mut total_scanned: u64 = 0;
    let mut skipped: u64 = 0;
    let mut submitted: u64 = 0;
    for path in file_rx {
        total_scanned += 1;

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

        let task = crate::embed::pool::IndexFileTask {
            path,
            kind: watcher::FileEventKind::Create,
            config: Arc::clone(config),
            db: Arc::clone(db),
            project_roots: Arc::clone(project_roots),
            project_overrides: Arc::clone(project_overrides),
        };
        // `rescan_workspace` doesn't currently receive a stats handle —
        // the rescan path is a minor submitter (only fires when config
        // adds a new workspace dir). The dominant scan + watcher paths
        // bump `files_submitted` at their submission sites. Threading
        // `stats` here would be additive but isn't needed for the
        // counter to be useful.
        if let Err(e) = embed_tx.send(EmbedIndexRequest::IndexFile(task)) {
            error!(error = %e, "Failed to submit IndexFile task during rescan");
            break;
        }
    }

    let _ = walk_handle.join();

    // Bump `last_scanned_at` for every project under this workspace —
    // catches the "rescan walked the tree, no files changed" case where
    // no per-file `upsert_project` would otherwise fire and the
    // freshness signal would stay stale forever.
    match rt_handle.block_on(db.update_projects_scanned_by_workspace(&workspace_path_str)) {
        Ok(rows) => {
            stats.last_scanned_writes.fetch_add(rows, Ordering::Relaxed);
        }
        Err(e) => {
            tracing::warn!(
                workspace = %workspace_path_str,
                error = %e,
                "Failed to update last_scanned_at after rescan"
            );
        }
    }

    info!(
        path = %workspace_path_str,
        total = total_scanned,
        unchanged = skipped,
        submitted,
        "Re-scan complete"
    );
}

// `handle_file_event` and `processor::process_file` are no longer needed:
// the inference-pool worker now owns the entire pipeline. See
// `src/embed/pool.rs::process_index_file_task`.
