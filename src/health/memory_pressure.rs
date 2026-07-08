//! `MemoryPressure` — the shared memory-pressure flag.
//!
//! Companion to [`crate::health::disk_pressure::DiskPressure`]: one instance on
//! [`crate::stats::tracker::StatsTracker`], written only by the memory-watchdog
//! loop ([`crate::health::watchdog::spawn_memory_watchdog`]) and read lock-free
//! by the heavy-cron gate and the embed-pool / ingest intake gates. When set,
//! pgmcp pauses its own memory-growing work (heavy crons + indexing) so a
//! transient RAM squeeze cannot escalate into the OOM kill that took the daemon
//! down repeatedly (`memory-graph-refresh` DB-saturation balloon, 2026-07-06).
//!
//! The hysteresis (the pause/resume floors, on both the available-RAM and the
//! process-RSS axes) lives in the watchdog's pure `mem_decide` function, not
//! here; this struct only records the resulting edge — an exact mirror of
//! [`crate::health::disk_pressure::DiskPressure`].

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Shared memory-pressure state. Written only by the watchdog; read everywhere.
#[derive(Debug)]
pub struct MemoryPressure {
    /// `true` = under pressure (tripped a pause floor, not yet recovered above
    /// the resume floors on both axes).
    paused: AtomicBool,
    /// Last observed system-wide available bytes (`/proc/meminfo:MemAvailable`).
    /// Purely observational (surfaced in `/api/status`); `u64::MAX` until the
    /// first poll.
    last_avail_bytes: AtomicU64,
    /// Last observed process resident-set size (`/proc/self/statm`). Purely
    /// observational; `0` until the first poll.
    last_rss_bytes: AtomicU64,
    /// Increments on every enter/exit edge.
    generation: AtomicU64,
}

/// A point-in-time view for `/api/status`.
#[derive(Debug, Clone, Copy)]
pub struct MemoryPressureSnapshot {
    pub paused: bool,
    pub last_avail_bytes: u64,
    pub last_rss_bytes: u64,
    pub generation: u64,
}

impl MemoryPressure {
    pub fn new() -> Self {
        Self {
            paused: AtomicBool::new(false),
            last_avail_bytes: AtomicU64::new(u64::MAX),
            last_rss_bytes: AtomicU64::new(0),
            generation: AtomicU64::new(0),
        }
    }

    /// The hot-path read.
    #[inline]
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    /// Record the latest observed axes (observational; does not change `paused`).
    pub fn record(&self, avail_bytes: u64, rss_bytes: u64) {
        self.last_avail_bytes.store(avail_bytes, Ordering::Release);
        self.last_rss_bytes.store(rss_bytes, Ordering::Release);
    }

    /// Enter pressure. Returns `true` only on the not-paused → paused edge.
    pub fn enter_pressure(&self) -> bool {
        let edge = self
            .paused
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        if edge {
            self.generation.fetch_add(1, Ordering::AcqRel);
        }
        edge
    }

    /// Exit pressure. Returns `true` only on the paused → not-paused edge.
    pub fn exit_pressure(&self) -> bool {
        let edge = self
            .paused
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        if edge {
            self.generation.fetch_add(1, Ordering::AcqRel);
        }
        edge
    }

    pub fn snapshot(&self) -> MemoryPressureSnapshot {
        MemoryPressureSnapshot {
            paused: self.paused.load(Ordering::Acquire),
            last_avail_bytes: self.last_avail_bytes.load(Ordering::Acquire),
            last_rss_bytes: self.last_rss_bytes.load(Ordering::Acquire),
            generation: self.generation.load(Ordering::Acquire),
        }
    }
}

impl Default for MemoryPressure {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_unpaused() {
        let m = MemoryPressure::new();
        assert!(!m.is_paused());
        assert_eq!(m.snapshot().last_avail_bytes, u64::MAX);
        assert_eq!(m.snapshot().last_rss_bytes, 0);
    }

    #[test]
    fn enter_exit_edges_fire_once() {
        let m = MemoryPressure::new();
        assert!(m.enter_pressure());
        assert!(m.is_paused());
        assert!(!m.enter_pressure(), "already paused: no further edge");
        assert!(m.exit_pressure());
        assert!(!m.is_paused());
        assert!(!m.exit_pressure(), "already resumed: no further edge");
        assert_eq!(m.snapshot().generation, 2);
    }

    #[test]
    fn record_is_observable() {
        let m = MemoryPressure::new();
        m.record(123 << 30, 45 << 30);
        let s = m.snapshot();
        assert_eq!(s.last_avail_bytes, 123 << 30);
        assert_eq!(s.last_rss_bytes, 45 << 30);
    }
}
