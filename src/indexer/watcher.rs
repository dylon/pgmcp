//! Filesystem watcher using notify v7.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crossbeam_channel::Sender;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tracing::{error, info};

use crate::stats::tracker::StatsTracker;

/// A file system event relevant to indexing.
#[derive(Debug, Clone)]
pub struct FileEvent {
    pub path: PathBuf,
    pub kind: FileEventKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileEventKind {
    Create,
    Modify,
    Remove,
}

/// Start watching all configured workspace paths.
/// Returns the watcher handle (dropping it stops watching).
pub fn start_watching(
    workspace_paths: &[String],
    event_tx: Sender<FileEvent>,
    stats: Arc<StatsTracker>,
) -> Result<RecommendedWatcher, notify::Error> {
    let tx = event_tx.clone();

    let mut watcher =
        notify::recommended_watcher(move |res: Result<Event, notify::Error>| match res {
            Ok(event) => {
                let kind = match event.kind {
                    EventKind::Create(_) => Some(FileEventKind::Create),
                    EventKind::Modify(_) => Some(FileEventKind::Modify),
                    EventKind::Remove(_) => Some(FileEventKind::Remove),
                    _ => None,
                };

                if let Some(kind) = kind {
                    for path in event.paths {
                        if path.is_file() || kind == FileEventKind::Remove {
                            stats
                                .watcher_events_received
                                .fetch_add(1, Ordering::Relaxed);
                            let _ = tx.send(FileEvent { path, kind });
                        }
                    }
                }
            }
            Err(e) => {
                error!(error = %e, "File watcher error");
            }
        })?;

    for workspace_path in workspace_paths {
        let path = Path::new(workspace_path);
        if path.exists() {
            watcher.watch(path, RecursiveMode::Recursive)?;
            info!(path = %workspace_path, "Watching workspace for changes");
        }
    }

    Ok(watcher)
}
