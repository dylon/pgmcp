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
            index_duration_ms: AtomicU64::new(0),
            embedding_duration_ms: AtomicU64::new(0),
            last_index_timestamp: AtomicU64::new(0),
            files_scanned: AtomicU64::new(0),
            files_skipped: AtomicU64::new(0),
            files_stale_removed: AtomicU64::new(0),
            active_work_pool_threads: AtomicU64::new(0),
            work_pool_queue_depth: AtomicU64::new(0),
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
            "index_duration_ms": self.index_duration_ms.load(Ordering::Acquire),
            "embedding_duration_ms": self.embedding_duration_ms.load(Ordering::Acquire),
            "files_scanned": self.files_scanned.load(Ordering::Acquire),
            "files_skipped": self.files_skipped.load(Ordering::Acquire),
            "files_stale_removed": self.files_stale_removed.load(Ordering::Acquire),
            "active_work_pool_threads": self.active_work_pool_threads.load(Ordering::Acquire),
            "work_pool_queue_depth": self.work_pool_queue_depth.load(Ordering::Acquire),
            "uptime_secs": self.uptime_start.elapsed().as_secs(),
        })
    }
}

impl Default for StatsTracker {
    fn default() -> Self {
        Self::new()
    }
}
