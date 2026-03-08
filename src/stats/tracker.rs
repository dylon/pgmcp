//! Lock-free atomic statistics tracker.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// All statistics counters — fully lock-free.
pub struct StatsTracker {
    // Indexing counters
    pub files_indexed: AtomicU64,
    pub files_failed: AtomicU64,
    pub chunks_embedded: AtomicU64,
    pub bytes_processed: AtomicU64,

    // MCP counters
    pub mcp_requests: AtomicU64,
    pub mcp_errors: AtomicU64,
    pub semantic_searches: AtomicU64,
    pub text_searches: AtomicU64,
    pub grep_searches: AtomicU64,
    pub commit_searches: AtomicU64,

    // Timing (cumulative)
    pub index_duration_ms: AtomicU64,
    pub embedding_duration_ms: AtomicU64,
    pub last_index_timestamp: AtomicU64,

    // Scan counters
    pub files_scanned: AtomicU64,
    pub files_skipped: AtomicU64,
    pub files_stale_removed: AtomicU64,

    // Pool state
    pub active_work_pool_threads: AtomicU64,
    pub work_pool_queue_depth: AtomicU64,

    // Cron counters
    pub cron_executions: AtomicU64,
    pub cron_panics: AtomicU64,

    // Git history counters
    pub git_commits_indexed: AtomicU64,
    pub git_commits_failed: AtomicU64,

    // Config watcher counters
    pub config_reloads: AtomicU64,
    pub config_reload_errors: AtomicU64,

    // Embedding pool counters
    pub embed_file_batches: AtomicU64,
    pub embed_commit_batches: AtomicU64,
    pub embed_errors: AtomicU64,

    // File watcher counters
    pub watcher_events_received: AtomicU64,
    pub watcher_events_filtered: AtomicU64,
    pub watcher_events_debounced: AtomicU64,

    // Work pool lifetime counters
    pub work_pool_tasks_completed: AtomicU64,
    pub work_pool_scale_ups: AtomicU64,
    pub work_pool_scale_downs: AtomicU64,

    // Uptime
    pub uptime_start: Instant,
}

impl StatsTracker {
    pub fn new() -> Self {
        Self {
            files_indexed: AtomicU64::new(0),
            files_failed: AtomicU64::new(0),
            chunks_embedded: AtomicU64::new(0),
            bytes_processed: AtomicU64::new(0),
            mcp_requests: AtomicU64::new(0),
            mcp_errors: AtomicU64::new(0),
            semantic_searches: AtomicU64::new(0),
            text_searches: AtomicU64::new(0),
            grep_searches: AtomicU64::new(0),
            commit_searches: AtomicU64::new(0),
            index_duration_ms: AtomicU64::new(0),
            embedding_duration_ms: AtomicU64::new(0),
            last_index_timestamp: AtomicU64::new(0),
            files_scanned: AtomicU64::new(0),
            files_skipped: AtomicU64::new(0),
            files_stale_removed: AtomicU64::new(0),
            active_work_pool_threads: AtomicU64::new(0),
            work_pool_queue_depth: AtomicU64::new(0),
            cron_executions: AtomicU64::new(0),
            cron_panics: AtomicU64::new(0),
            git_commits_indexed: AtomicU64::new(0),
            git_commits_failed: AtomicU64::new(0),
            config_reloads: AtomicU64::new(0),
            config_reload_errors: AtomicU64::new(0),
            embed_file_batches: AtomicU64::new(0),
            embed_commit_batches: AtomicU64::new(0),
            embed_errors: AtomicU64::new(0),
            watcher_events_received: AtomicU64::new(0),
            watcher_events_filtered: AtomicU64::new(0),
            watcher_events_debounced: AtomicU64::new(0),
            work_pool_tasks_completed: AtomicU64::new(0),
            work_pool_scale_ups: AtomicU64::new(0),
            work_pool_scale_downs: AtomicU64::new(0),
            uptime_start: Instant::now(),
        }
    }

    /// Get a JSON snapshot of all counters.
    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "files_indexed": self.files_indexed.load(Ordering::Acquire),
            "files_failed": self.files_failed.load(Ordering::Acquire),
            "chunks_embedded": self.chunks_embedded.load(Ordering::Acquire),
            "bytes_processed": self.bytes_processed.load(Ordering::Acquire),
            "mcp_requests": self.mcp_requests.load(Ordering::Acquire),
            "mcp_errors": self.mcp_errors.load(Ordering::Acquire),
            "semantic_searches": self.semantic_searches.load(Ordering::Acquire),
            "text_searches": self.text_searches.load(Ordering::Acquire),
            "grep_searches": self.grep_searches.load(Ordering::Acquire),
            "commit_searches": self.commit_searches.load(Ordering::Acquire),
            "index_duration_ms": self.index_duration_ms.load(Ordering::Acquire),
            "embedding_duration_ms": self.embedding_duration_ms.load(Ordering::Acquire),
            "files_scanned": self.files_scanned.load(Ordering::Acquire),
            "files_skipped": self.files_skipped.load(Ordering::Acquire),
            "files_stale_removed": self.files_stale_removed.load(Ordering::Acquire),
            "active_work_pool_threads": self.active_work_pool_threads.load(Ordering::Acquire),
            "work_pool_queue_depth": self.work_pool_queue_depth.load(Ordering::Acquire),
            "cron_executions": self.cron_executions.load(Ordering::Acquire),
            "cron_panics": self.cron_panics.load(Ordering::Acquire),
            "git_commits_indexed": self.git_commits_indexed.load(Ordering::Acquire),
            "git_commits_failed": self.git_commits_failed.load(Ordering::Acquire),
            "config_reloads": self.config_reloads.load(Ordering::Acquire),
            "config_reload_errors": self.config_reload_errors.load(Ordering::Acquire),
            "embed_file_batches": self.embed_file_batches.load(Ordering::Acquire),
            "embed_commit_batches": self.embed_commit_batches.load(Ordering::Acquire),
            "embed_errors": self.embed_errors.load(Ordering::Acquire),
            "watcher_events_received": self.watcher_events_received.load(Ordering::Acquire),
            "watcher_events_filtered": self.watcher_events_filtered.load(Ordering::Acquire),
            "watcher_events_debounced": self.watcher_events_debounced.load(Ordering::Acquire),
            "work_pool_tasks_completed": self.work_pool_tasks_completed.load(Ordering::Acquire),
            "work_pool_scale_ups": self.work_pool_scale_ups.load(Ordering::Acquire),
            "work_pool_scale_downs": self.work_pool_scale_downs.load(Ordering::Acquire),
            "uptime_secs": self.uptime_start.elapsed().as_secs(),
        })
    }
}

impl Default for StatsTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snapshot_contains_all_new_counters() {
        let stats = StatsTracker::new();

        // Set all new fields to distinct non-zero values
        stats.cron_executions.store(1, Ordering::Relaxed);
        stats.cron_panics.store(2, Ordering::Relaxed);
        stats.git_commits_indexed.store(3, Ordering::Relaxed);
        stats.git_commits_failed.store(4, Ordering::Relaxed);
        stats.config_reloads.store(5, Ordering::Relaxed);
        stats.config_reload_errors.store(6, Ordering::Relaxed);
        stats.embed_file_batches.store(7, Ordering::Relaxed);
        stats.embed_commit_batches.store(8, Ordering::Relaxed);
        stats.embed_errors.store(9, Ordering::Relaxed);
        stats.watcher_events_received.store(10, Ordering::Relaxed);
        stats.watcher_events_filtered.store(11, Ordering::Relaxed);
        stats.watcher_events_debounced.store(12, Ordering::Relaxed);
        stats.work_pool_tasks_completed.store(13, Ordering::Relaxed);
        stats.work_pool_scale_ups.store(14, Ordering::Relaxed);
        stats.work_pool_scale_downs.store(15, Ordering::Relaxed);
        stats.commit_searches.store(16, Ordering::Relaxed);

        let snap = stats.snapshot();

        assert_eq!(snap["cron_executions"], 1);
        assert_eq!(snap["cron_panics"], 2);
        assert_eq!(snap["git_commits_indexed"], 3);
        assert_eq!(snap["git_commits_failed"], 4);
        assert_eq!(snap["config_reloads"], 5);
        assert_eq!(snap["config_reload_errors"], 6);
        assert_eq!(snap["embed_file_batches"], 7);
        assert_eq!(snap["embed_commit_batches"], 8);
        assert_eq!(snap["embed_errors"], 9);
        assert_eq!(snap["watcher_events_received"], 10);
        assert_eq!(snap["watcher_events_filtered"], 11);
        assert_eq!(snap["watcher_events_debounced"], 12);
        assert_eq!(snap["work_pool_tasks_completed"], 13);
        assert_eq!(snap["work_pool_scale_ups"], 14);
        assert_eq!(snap["work_pool_scale_downs"], 15);
        assert_eq!(snap["commit_searches"], 16);

        // Verify existing fields still present
        assert_eq!(snap["files_indexed"], 0);
        assert_eq!(snap["mcp_requests"], 0);
        assert!(snap["uptime_secs"].as_u64().is_some());
    }
}
