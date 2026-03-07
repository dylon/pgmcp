//! Event processor: wires reactive pipeline for file indexing.
//!
//! Watcher events -> filter -> debounce -> WorkPool dispatch
//! Scanner paths -> map -> WorkPool dispatch (low priority)

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use crossbeam_channel::Sender;
use dashmap::DashMap;
use tracing::{error, info};

use crate::config::Config;
use crate::embed::pool::EmbedRequest;
use crate::indexer::{scanner, watcher};
use crate::reactive::operators;
use crate::shutdown::ShutdownCoordinator;
use crate::stats::tracker::StatsTracker;
use crate::work_pool::pool::{Priority, WorkPool};

/// Handle returned from start_indexing. Dropping it stops the file watcher.
pub struct IndexerHandle {
    _watcher: notify::RecommendedWatcher,
    _subscriptions: Vec<crate::reactive::subscription::Subscription>,
}

/// Start the full indexing pipeline.
pub fn start_indexing(
    config: Arc<ArcSwap<Config>>,
    db_pool: sqlx::PgPool,
    work_pool: Arc<WorkPool>,
    embed_tx: Sender<EmbedRequest>,
    stats: Arc<StatsTracker>,
    shutdown: ShutdownCoordinator,
) -> Result<IndexerHandle, crate::error::PgmcpError> {
    let config_snapshot = config.load();
    let project_roots: Arc<DashMap<PathBuf, scanner::ProjectRoot>> = Arc::new(DashMap::new());

    // Capture the tokio runtime handle so WorkPool threads can run async code.
    // This must be called while we're on a tokio runtime thread (which we are,
    // since start_indexing is called from run_server inside #[tokio::main]).
    let rt_handle = tokio::runtime::Handle::current();

    // 1. Start file watcher
    let (event_tx, event_rx) = crossbeam_channel::bounded(4096);
    let watcher_handle = watcher::start_watching(
        &config_snapshot.workspace.paths,
        event_tx,
    )?;

    // 2. Set up reactive pipeline for watcher events
    let config_for_filter = Arc::clone(&config);
    let filtered_rx = {
        let rx = event_rx;
        // Filter to only configured extensions and non-excluded paths
        let (tx, filtered_rx) = crossbeam_channel::bounded(2048);

        let shutdown_flag = shutdown.terminating_flag();
        std::thread::Builder::new()
            .name("pgmcp-event-filter".into())
            .spawn(move || {
                for event in rx {
                    if shutdown_flag.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }

                    let cfg = config_for_filter.load();

                    // Skip non-configured extensions
                    if event.kind != watcher::FileEventKind::Remove
                        && !cfg.indexer.is_configured_extension(&event.path)
                    {
                        continue;
                    }

                    // Skip excluded patterns
                    let path_str = event.path.to_string_lossy();
                    let excluded = cfg.indexer.exclude_patterns.iter().any(|pattern| {
                        if pattern.starts_with('*') {
                            path_str.ends_with(&pattern[1..])
                        } else {
                            path_str.contains(pattern)
                        }
                    });

                    if excluded {
                        continue;
                    }

                    if tx.send(event).is_err() {
                        break;
                    }
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
    let project_roots_for_events = Arc::clone(&project_roots);

    let rt_for_events = rt_handle.clone();
    let event_sub = crate::reactive::observable::Observable::from_receiver(debounced_rx)
        .subscribe(move |event: watcher::FileEvent| {
            let path = event.path.clone();
            let config = Arc::clone(&config_for_events);
            let db = db_for_events.clone();
            let embed_tx = embed_tx_for_events.clone();
            let stats = Arc::clone(&stats_for_events);
            let roots = Arc::clone(&project_roots_for_events);
            let rt = rt_for_events.clone();

            work_pool_for_events.submit(
                move || {
                    rt.block_on(async {
                        handle_file_event(&path, &event.kind, &config.load(), &db, &embed_tx, &stats, &roots).await;
                    });
                },
                Priority::High,
            );
        });

    // 5. Start initial scan in background
    let config_for_scan = Arc::clone(&config);
    let work_pool_for_scan = Arc::clone(&work_pool);
    let db_for_scan = db_pool.clone();
    let embed_tx_for_scan = embed_tx.clone();
    let stats_for_scan = Arc::clone(&stats);
    let project_roots_for_scan = Arc::clone(&project_roots);
    let rt_for_scan = rt_handle;

    std::thread::Builder::new()
        .name("pgmcp-scanner".into())
        .spawn(move || {
            let (file_tx, file_rx) = crossbeam_channel::bounded(4096);
            let config_snapshot = config_for_scan.load();

            // Load indexed file metadata for scan optimization (Level 1 skip)
            let metadata_map: std::collections::HashMap<String, crate::db::queries::IndexedFileMeta> =
                match rt_for_scan.block_on(crate::db::queries::get_all_file_metadata(&db_for_scan)) {
                    Ok(metas) => {
                        let len = metas.len();
                        let mut map = std::collections::HashMap::with_capacity(len);
                        for meta in metas {
                            map.insert(meta.path.clone(), meta);
                        }
                        info!(indexed_files = len, "Loaded file metadata for scan optimization");
                        map
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to load file metadata, falling back to full scan");
                        std::collections::HashMap::new()
                    }
                };

            // Walk directories in parallel
            let scan_config = config_snapshot.clone();
            let scan_roots = Arc::clone(&project_roots_for_scan);
            let scan_handle = std::thread::Builder::new()
                .name("pgmcp-scan-walk".into())
                .spawn(move || {
                    scanner::scan_workspaces(&scan_config, file_tx, &scan_roots);
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
                if let Some(db_meta) = metadata_map.get(&*path.to_string_lossy()) {
                    if let Ok(fs_meta) = std::fs::metadata(&path) {
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
                }

                submitted += 1;

                let config = Arc::clone(&config_for_scan);
                let db = db_for_scan.clone();
                let embed_tx = embed_tx_for_scan.clone();
                let stats = Arc::clone(&stats_for_scan);
                let roots = Arc::clone(&project_roots_for_scan);
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
                match rt_for_scan.block_on(
                    crate::db::queries::delete_files_batch(&db_for_scan, &stale_paths),
                ) {
                    Ok(deleted) => {
                        info!(detected = stale_count, deleted, "Removed stale files from index");
                    }
                    Err(e) => {
                        error!(count = stale_count, error = %e, "Failed to remove stale files");
                    }
                }
                stats_for_scan
                    .files_stale_removed
                    .fetch_add(stale_count, std::sync::atomic::Ordering::Relaxed);
            }

            stats_for_scan
                .files_scanned
                .fetch_add(total_scanned, std::sync::atomic::Ordering::Relaxed);
            stats_for_scan
                .files_skipped
                .fetch_add(skipped, std::sync::atomic::Ordering::Relaxed);

            info!(
                total = total_scanned,
                unchanged = skipped,
                submitted,
                stale_removed = stale_count,
                "Initial scan complete"
            );
        })
        .expect("Failed to spawn scanner thread");

    Ok(IndexerHandle {
        _watcher: watcher_handle,
        _subscriptions: vec![event_sub],
    })
}

async fn handle_file_event(
    path: &PathBuf,
    kind: &watcher::FileEventKind,
    config: &Config,
    db_pool: &sqlx::PgPool,
    embed_tx: &Sender<EmbedRequest>,
    stats: &StatsTracker,
    project_roots: &DashMap<PathBuf, scanner::ProjectRoot>,
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
            let (project_id, workspace_path) =
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
                            Ok(id) => (id, root_path.to_string_lossy().into_owned()),
                            Err(e) => {
                                error!(error = %e, "Failed to upsert project");
                                return;
                            }
                        }
                    }
                    None => {
                        // No project root found, use the workspace path
                        let workspace = config
                            .workspace
                            .paths
                            .first()
                            .cloned()
                            .unwrap_or_default();
                        match crate::db::queries::upsert_project(
                            db_pool,
                            &workspace,
                            &workspace,
                            "default",
                        )
                        .await
                        {
                            Ok(id) => (id, workspace),
                            Err(e) => {
                                error!(error = %e, "Failed to upsert default project");
                                return;
                            }
                        }
                    }
                };

            if let Err(e) = super::processor::process_file(
                path,
                project_id,
                &workspace_path,
                config,
                db_pool,
                embed_tx,
                stats,
            )
            .await
            {
                let path_str = path.to_string_lossy();
                error!(path = %path_str, error = %e, "Failed to process file");
                stats
                    .files_failed
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
}
