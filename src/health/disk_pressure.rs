//! `DiskPressure` — the shared disk-space-pressure flag.
//!
//! Companion to [`crate::health::db_health::DbHealth`]: one instance on
//! [`crate::stats::tracker::StatsTracker`], written only by the watchdog loop
//! ([`crate::health::watchdog`]) and read lock-free by the embed-pool intake
//! gate and the heavy-cron gate. When set, pgmcp pauses its **own** disk-growing
//! work (indexing + heavy crons) so it cannot contribute to a fill — the same
//! failure that took PostgreSQL down on 2026-06-11.
//!
//! Hysteresis (the pause/resume floors) lives in the watchdog's pure `decide`
//! function, not here; this struct only records the resulting edge.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Shared disk-pressure state. Written only by the watchdog; read everywhere.
#[derive(Debug)]
pub struct DiskPressure {
    /// `true` = under pressure (below `pause_floor`, not yet recovered above
    /// `resume_floor`).
    paused: AtomicBool,
    /// Last observed minimum available bytes across the watched filesystems.
    /// Purely observational (surfaced in `/api/status`); `u64::MAX` until the
    /// first poll.
    last_avail_bytes: AtomicU64,
    /// Increments on every enter/exit edge.
    generation: AtomicU64,
}

/// A point-in-time view for `/api/status`.
#[derive(Debug, Clone, Copy)]
pub struct DiskPressureSnapshot {
    pub paused: bool,
    pub last_avail_bytes: u64,
    pub generation: u64,
}

impl DiskPressure {
    pub fn new() -> Self {
        Self {
            paused: AtomicBool::new(false),
            last_avail_bytes: AtomicU64::new(u64::MAX),
            generation: AtomicU64::new(0),
        }
    }

    /// The hot-path read.
    #[inline]
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    pub fn record_avail(&self, bytes: u64) {
        self.last_avail_bytes.store(bytes, Ordering::Release);
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

    pub fn snapshot(&self) -> DiskPressureSnapshot {
        DiskPressureSnapshot {
            paused: self.paused.load(Ordering::Acquire),
            last_avail_bytes: self.last_avail_bytes.load(Ordering::Acquire),
            generation: self.generation.load(Ordering::Acquire),
        }
    }
}

impl Default for DiskPressure {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_unpaused() {
        let d = DiskPressure::new();
        assert!(!d.is_paused());
        assert_eq!(d.snapshot().last_avail_bytes, u64::MAX);
    }

    #[test]
    fn enter_exit_edges_fire_once() {
        let d = DiskPressure::new();
        assert!(d.enter_pressure());
        assert!(d.is_paused());
        assert!(!d.enter_pressure(), "already paused: no further edge");
        assert!(d.exit_pressure());
        assert!(!d.is_paused());
        assert!(!d.exit_pressure(), "already resumed: no further edge");
        assert_eq!(d.snapshot().generation, 2);
    }

    #[test]
    fn record_avail_is_observable() {
        let d = DiskPressure::new();
        d.record_avail(123 << 30);
        assert_eq!(d.snapshot().last_avail_bytes, 123 << 30);
    }
}
