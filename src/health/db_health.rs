//! `DbHealth` — the shared DB-availability circuit-breaker state.
//!
//! One instance lives on [`crate::stats::tracker::StatsTracker`] (already
//! `Arc`-threaded into the embed pool, the cron scheduler, every light-cron
//! closure, and the REST `/health` handler), so every DB-using subsystem can
//! consult it lock-free and short-circuit *before* paying a 10-second
//! `acquire_timeout` against a database that is down.
//!
//! ## Why this exists
//!
//! On 2026-06-11 the host disk filled, PostgreSQL PANIC'd on ENOSPC and stayed
//! down ~2 h (systemd `Restart=no`). pgmcp had no shared notion of "the DB is
//! down", so **every** operation independently timed out after 10 s and logged
//! an error every interval — **1447 `PoolTimedOut` lines** for a single outage.
//! This breaker collapses that to two lines (one Up→Down, one Down→Up) and lets
//! consumers skip work quietly instead of stalling.
//!
//! ## Concurrency model
//!
//! All fields are atomics; there is **one writer** (the prober loop in
//! [`crate::health::prober`]) and many lock-free readers. The Up↔Down edges are
//! resolved with `compare_exchange`, so exactly one caller observes each
//! transition — the basis for "log the transition exactly once".

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Shared DB-availability state. Written only by the prober; read everywhere.
#[derive(Debug)]
pub struct DbHealth {
    /// `true` = the last probe (or the optimistic initial state) is Up.
    up: AtomicBool,
    /// Unix epoch seconds of the Up→Down transition; `0` while up.
    down_since_epoch: AtomicU64,
    /// Increments on **every** edge (Up→Down and Down→Up). Lets a reader detect
    /// "did anything change" without racing two loads, and is the basis of the
    /// "exactly one transition" test assertions.
    generation: AtomicU64,
}

/// A consistent-enough point-in-time view for `/health` and `/api/status`.
#[derive(Debug, Clone, Copy)]
pub struct DbHealthSnapshot {
    pub up: bool,
    /// `0` when up; otherwise the epoch seconds the outage began.
    pub down_since_epoch: u64,
    pub generation: u64,
}

impl DbHealth {
    /// Start **optimistic** (`up = true`). The pool and migrations have already
    /// succeeded by the time this is constructed, and an optimistic start means
    /// consumers do not all short-circuit during the first probe interval before
    /// the prober has run once.
    pub fn new() -> Self {
        Self {
            up: AtomicBool::new(true),
            down_since_epoch: AtomicU64::new(0),
            generation: AtomicU64::new(0),
        }
    }

    /// The hot-path read. Lock-free `Acquire` load.
    #[inline]
    pub fn is_up(&self) -> bool {
        self.up.load(Ordering::Acquire)
    }

    /// Record a failed probe. Returns `true` **only** on the Up→Down edge (so
    /// the caller logs exactly one "database unreachable" line); `false` while
    /// already down. Stamps `down_since_epoch` on the edge.
    pub fn record_failure(&self) -> bool {
        if self
            .up
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.down_since_epoch.store(now_epoch(), Ordering::Release);
            self.generation.fetch_add(1, Ordering::AcqRel);
            true
        } else {
            false
        }
    }

    /// Record a successful probe. Returns `Some(down_duration_secs)` **only** on
    /// the Down→Up edge (so the caller logs recovery once and triggers outbox
    /// replay); `None` while already up.
    pub fn record_success(&self) -> Option<u64> {
        if self
            .up
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let since = self.down_since_epoch.swap(0, Ordering::AcqRel);
            self.generation.fetch_add(1, Ordering::AcqRel);
            Some(now_epoch().saturating_sub(since))
        } else {
            None
        }
    }

    pub fn snapshot(&self) -> DbHealthSnapshot {
        DbHealthSnapshot {
            up: self.up.load(Ordering::Acquire),
            down_since_epoch: self.down_since_epoch.load(Ordering::Acquire),
            generation: self.generation.load(Ordering::Acquire),
        }
    }
}

impl Default for DbHealth {
    fn default() -> Self {
        Self::new()
    }
}

/// Current Unix epoch seconds, saturating to 0 on a pre-epoch clock.
fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn starts_optimistic_up() {
        let h = DbHealth::new();
        assert!(h.is_up());
        assert_eq!(h.snapshot().down_since_epoch, 0);
        assert_eq!(h.snapshot().generation, 0);
    }

    #[test]
    fn failure_edge_fires_once() {
        let h = DbHealth::new();
        assert!(h.record_failure(), "first failure is the Up→Down edge");
        assert!(!h.is_up());
        assert!(h.snapshot().down_since_epoch > 0);
        assert!(!h.record_failure(), "already down: no further edge");
        assert_eq!(h.snapshot().generation, 1);
    }

    #[test]
    fn success_edge_returns_down_duration_once() {
        let h = DbHealth::new();
        h.record_failure();
        let down = h.record_success();
        assert!(down.is_some(), "Down→Up edge returns a duration");
        assert!(h.is_up());
        assert_eq!(h.snapshot().down_since_epoch, 0);
        assert!(h.record_success().is_none(), "already up: no further edge");
    }

    #[test]
    fn generation_counts_every_edge() {
        let h = DbHealth::new();
        h.record_failure(); // 1
        h.record_success(); // 2
        h.record_failure(); // 3
        assert_eq!(h.snapshot().generation, 3);
    }

    #[test]
    fn flap_sequence_counts_each_edge() {
        let h = DbHealth::new();
        for _ in 0..5 {
            assert!(h.record_failure());
            assert!(h.record_success().is_some());
        }
        assert_eq!(h.snapshot().generation, 10);
        assert!(h.is_up());
    }

    #[test]
    fn concurrent_failures_admit_exactly_one_edge() {
        // Many threads race to fail an up breaker; exactly one wins the edge.
        let h = Arc::new(DbHealth::new());
        let n = 32;
        let mut handles = Vec::with_capacity(n);
        for _ in 0..n {
            let h = Arc::clone(&h);
            handles.push(thread::spawn(move || h.record_failure()));
        }
        let edges = handles
            .into_iter()
            .map(|j| j.join().expect("join"))
            .filter(|&won| won)
            .count();
        assert_eq!(edges, 1, "exactly one thread observed the Up→Down edge");
        assert!(!h.is_up());
        assert_eq!(h.snapshot().generation, 1);
    }
}
