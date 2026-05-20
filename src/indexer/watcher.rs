//! Filesystem watcher using notify v7.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crossbeam_channel::Sender;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tracing::{error, info};

use crate::indexer::config_watcher::WatcherCommand;
use crate::stats::tracker::StatsTracker;

/// Substrings that identify an inotify-queue-overflow error in the
/// `notify::Error::Display` representation. notify v7 surfaces overflow
/// as a `Generic(String)` rather than a typed variant, so we pattern-match
/// the message text. The kernel emits `IN_Q_OVERFLOW` when
/// `fs.inotify.max_queued_events` is exceeded; once that happens, the
/// existing watcher drops further events forever. The callback responds
/// by dispatching `WatcherCommand::Reinit(workspaces)`, which the
/// indexer's cmd thread acts on by building a fresh
/// `RecommendedWatcher` and enqueuing a per-workspace rescan to catch
/// the events that were lost during the overflow window.
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
///
/// Returns the watcher handle (dropping it stops watching). The optional
/// `watcher_cmd_tx` is the channel the callback uses to request an
/// automatic re-arm on inotify queue overflow — pass `Some(...)` from
/// the daemon's indexer (which owns the receiving end and can rebuild
/// the watcher) and `None` from tests or one-shot callers that don't
/// run the cmd thread.
pub fn start_watching(
    workspace_paths: &[String],
    event_tx: Sender<FileEvent>,
    stats: Arc<StatsTracker>,
    watcher_cmd_tx: Option<Sender<WatcherCommand>>,
) -> Result<RecommendedWatcher, notify::Error> {
    let tx = event_tx.clone();
    let workspaces_for_cb: Vec<PathBuf> = workspace_paths.iter().map(PathBuf::from).collect();
    let cmd_tx_for_cb = watcher_cmd_tx.clone();

    let stats_for_cb = Arc::clone(&stats);
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
                // notifications. When the daemon supplies a
                // `watcher_cmd_tx`, dispatch a `Reinit` so the cmd
                // thread rebuilds the watcher and re-scans the affected
                // workspaces; either way emit a loud error so operators
                // also see the condition.
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
                        "File watcher: inotify queue OVERFLOW — events lost; \
                         requesting watcher re-arm. \
                         Raise fs.inotify.max_queued_events via sysctl to mitigate."
                    );
                    if let Some(cmd_tx) = &cmd_tx_for_cb {
                        let _ = cmd_tx.send(WatcherCommand::Reinit(workspaces_for_cb.clone()));
                    }
                } else {
                    error!(error = %e, "File watcher error");
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::unbounded;

    #[test]
    fn overflow_hints_classify_common_message_shapes() {
        // The exact strings notify v7 surfaces vary across kernel
        // versions; we match a few canonical shapes. If kernel changes
        // surface a new wording, add it here.
        assert!(
            OVERFLOW_HINTS
                .iter()
                .any(|h| "inotify queue overflow detected".contains(h))
        );
        assert!(
            OVERFLOW_HINTS
                .iter()
                .any(|h| "IN_Q_OVERFLOW: event queue exhausted".contains(h))
        );
        assert!(
            OVERFLOW_HINTS
                .iter()
                .any(|h| "kernel reported Q_OVERFLOW".contains(h))
        );
    }

    #[test]
    fn reinit_command_carries_workspace_list() {
        // Round-trip a `Reinit` through the channel to confirm the new
        // variant constructs cleanly and is shape-compatible with the
        // command thread's iterator.
        let (tx, rx) = unbounded::<WatcherCommand>();
        let paths = vec![PathBuf::from("/ws/a"), PathBuf::from("/ws/b")];
        tx.send(WatcherCommand::Reinit(paths.clone()))
            .expect("send must succeed on a live channel");
        match rx.recv().expect("recv") {
            WatcherCommand::Reinit(received) => assert_eq!(received, paths),
            other => panic!("expected Reinit, got {:?}", std::mem::discriminant(&other)),
        }
    }
}
