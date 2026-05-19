//! Filesystem watcher using notify v7.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crossbeam_channel::Sender;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tracing::{error, info};

use crate::stats::tracker::StatsTracker;

/// Substrings that identify an inotify-queue-overflow error in the
/// `notify::Error::Display` representation. notify v7 surfaces overflow
/// as a `Generic(String)` rather than a typed variant, so we pattern-match
/// the message text. The kernel emits `IN_Q_OVERFLOW` when
/// `fs.inotify.max_queued_events` is exceeded; once that happens, the
/// watcher drops further events and the workspace silently goes stale
/// until the watcher is re-armed. The current implementation only logs
/// loudly; a future change will trigger an automatic re-arm.
const OVERFLOW_HINTS: &[&str] = &["queue overflow", "Q_OVERFLOW", "IN_Q_OVERFLOW"];

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

    let stats_for_cb = Arc::clone(&stats);
    let mut watcher = notify::recommended_watcher(
        move |res: Result<Event, notify::Error>| match res {
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
                            stats_for_cb
                                .watcher_events_received
                                .fetch_add(1, Ordering::Relaxed);
                            let _ = tx.send(FileEvent { path, kind });
                        }
                    }
                }
            }
            Err(e) => {
                // Inotify queue overflow (Linux `IN_Q_OVERFLOW`) silently
                // truncates the event stream until the watcher is re-armed —
                // the project then diverges from disk with no further
                // notifications. Surface this loudly so an operator can
                // act before too many files drift out of the index. Full
                // automatic re-arm is a separate follow-up; this counter
                // and the error-level log are the minimum visibility hook.
                let msg = e.to_string();
                let is_overflow = OVERFLOW_HINTS.iter().any(|hint| msg.contains(hint));
                stats_for_cb
                    .watcher_errors_total
                    .fetch_add(1, Ordering::Relaxed);
                if is_overflow {
                    stats_for_cb
                        .inotify_overflows_total
                        .fetch_add(1, Ordering::Relaxed);
                    error!(
                        error = %e,
                        "File watcher: inotify queue OVERFLOW — events lost; workspace index will drift until daemon restart. \
                         Raise fs.inotify.max_queued_events via sysctl to mitigate."
                    );
                } else {
                    error!(error = %e, "File watcher error");
                }
            }
        },
    )?;

    for workspace_path in workspace_paths {
        let path = Path::new(workspace_path);
        if path.exists() {
            watcher.watch(path, RecursiveMode::Recursive)?;
            info!(path = %workspace_path, "Watching workspace for changes");
        }
    }

    Ok(watcher)
}
