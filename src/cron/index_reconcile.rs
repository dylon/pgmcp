//! Reconcile-backstop cron: periodically re-walk every workspace with the
//! Level-1 stat-only skip so live-watcher events that were *missed* self-heal
//! within one interval instead of waiting for a daemon restart.
//!
//! ## Why this exists
//!
//! Freshness is normally delivered by the live `notify` (inotify) watcher
//! (`src/indexer/watcher.rs`) → 300 ms debounce → embed pool. That path is fast
//! and correct, but an event can be *missed*: an inotify queue overflow past the
//! re-arm, an editor that saves atomically by writing a temp file and renaming
//! it with preserved metadata, the daemon being down during an edit, or the
//! ADR-015 intake-gate-closed window during a DB outage. When an event is
//! missed, nothing re-checks that file until the next **daemon restart** —
//! `stale-cleanup` only handles deletions, not modifications.
//!
//! This cron closes that gap. It sends one [`WatcherCommand::Rescan`] per
//! workspace root; the watcher-command thread serializes them through
//! `rescan_workspace`, which applies the same Level-1 `(size,mtime)` skip and
//! the bounded-failure gate, so only genuinely changed/new files are re-read and
//! re-embedded. A reconcile pass is therefore O(stat) over the corpus plus the
//! cost of the (usually empty) changed set — cheap enough to run every
//! `[cron] index_reconcile_interval_secs` (default 30 min).
//!
//! Complementary to, not a replacement for, `integrity-check` (which only GCs
//! `content_hash IS NULL` rows) and `stale-cleanup` (deletions only).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crossbeam_channel::Sender;

use crate::indexer::config_watcher::WatcherCommand;
use crate::stats::tracker::StatsTracker;

/// Enqueue one `WatcherCommand::Rescan` per workspace root. Synchronous and
/// fast — the actual walk happens later, serialized in the watcher-command
/// thread. Bumps `index_reconcile_runs` (the run reached the work-eligible
/// state). Best-effort per workspace: a full command channel logs and is
/// retried on the next tick.
pub fn run_or_log(
    watcher_cmd_tx: &Sender<WatcherCommand>,
    workspaces: &[PathBuf],
    stats: &Arc<StatsTracker>,
) {
    stats.index_reconcile_runs.fetch_add(1, Ordering::Relaxed);
    let mut enqueued = 0usize;
    for ws in workspaces {
        match watcher_cmd_tx.try_send(WatcherCommand::Rescan(ws.clone())) {
            Ok(()) => enqueued += 1,
            Err(e) => {
                tracing::warn!(
                    workspace = %ws.display(),
                    error = %e,
                    "index-reconcile: failed to enqueue rescan (command channel full?)"
                );
            }
        }
    }
    tracing::debug!(
        workspaces = workspaces.len(),
        enqueued,
        "index-reconcile: enqueued workspace rescans"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn run_or_log_enqueues_one_rescan_per_workspace_and_bumps_counter() {
        let (tx, rx) = crossbeam_channel::bounded(16);
        let stats = Arc::new(StatsTracker::new());
        let workspaces = vec![PathBuf::from("/ws/a"), PathBuf::from("/ws/b")];

        run_or_log(&tx, &workspaces, &stats);

        assert_eq!(
            stats.index_reconcile_runs.load(Ordering::Relaxed),
            1,
            "the run is counted"
        );
        let mut got: Vec<PathBuf> = Vec::new();
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                WatcherCommand::Rescan(p) => got.push(p),
                _ => panic!("unexpected non-Rescan command enqueued"),
            }
        }
        assert_eq!(got.len(), 2, "one Rescan per workspace");
        assert!(got.contains(&PathBuf::from("/ws/a")));
        assert!(got.contains(&PathBuf::from("/ws/b")));
    }

    #[test]
    fn run_or_log_with_no_workspaces_still_counts_the_run() {
        let (tx, rx) = crossbeam_channel::bounded::<WatcherCommand>(4);
        let stats = Arc::new(StatsTracker::new());
        run_or_log(&tx, &[], &stats);
        assert_eq!(stats.index_reconcile_runs.load(Ordering::Relaxed), 1);
        assert!(rx.try_recv().is_err(), "no commands enqueued");
    }
}
