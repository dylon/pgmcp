//! Linux `/proc`-based RSS and memory-availability helpers.
//!
//! Used by heavy cron jobs to:
//! - log `rss_mb_start / end / delta` per run (scientific ledger)
//! - pre-flight memory budget before running global FCM
//! - feed a peak-RSS sampler thread for Prometheus gauge export

use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::stats::tracker::StatsTracker;

#[allow(unused_imports)]
use std::sync::atomic::AtomicU64;

/// Current resident set size in bytes, via `/proc/self/statm`.
/// Returns `None` on non-Linux or if the file can't be read.
pub fn current_rss_bytes() -> Option<u64> {
    let data = fs::read_to_string("/proc/self/statm").ok()?;
    let resident_pages: u64 = data.split_whitespace().nth(1)?.parse().ok()?;
    let page_size = page_size_bytes();
    Some(resident_pages * page_size)
}

/// Current OS thread count of this process, via `/proc/self/task`.
///
/// Returns `None` on non-Linux or if the directory can't be read. Heavy cron
/// jobs log a per-run `threads_delta` from this so a background-thread leak
/// (e.g. the persistent-trie daemon-thread leak) shows up as a steadily
/// climbing thread count rather than silently as RSS growth.
pub fn current_thread_count() -> Option<u64> {
    let count = fs::read_dir("/proc/self/task")
        .ok()?
        .filter(|e| e.is_ok())
        .count();
    Some(count as u64)
}

/// System-wide available memory in bytes, via `/proc/meminfo:MemAvailable`.
/// Returns `None` on non-Linux or if the field isn't present.
pub fn mem_available_bytes() -> Option<u64> {
    let data = fs::read_to_string("/proc/meminfo").ok()?;
    for line in data.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb * 1024);
        }
    }
    None
}

/// Return free-but-retained heap memory to the OS via glibc `malloc_trim(0)`.
///
/// glibc keeps each malloc arena's high-water mark for the life of the process
/// (see `cap_malloc_arenas` in `main.rs`), so a transient allocation burst — e.g.
/// the in-flight request/result buffers that pile up while a heavy cron saturates
/// the database for minutes — inflates RSS with memory that is *free* but never
/// handed back to the kernel. With `vm.swappiness=0` that retained anonymous RSS
/// is never swapped either, so it stacks across runs until the OOM killer fires.
/// Calling this after heavy work (and on the memory-watchdog pressure edge)
/// releases the top of the main arena plus any fully-free `mmap`'d chunks,
/// converting the retention from cumulative-and-fatal to transient-and-reclaimed.
///
/// Cost is an arena-lock walk (tens of ms on a large heap), so call it off hot
/// paths only. No-op on non-glibc targets (`malloc_trim` is a GNU extension).
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn trim_malloc() {
    // SAFETY: `malloc_trim` takes a pad byte count and is always safe to call;
    // the returned c_int (1 = memory released, 0 = none) is advisory only.
    unsafe {
        libc::malloc_trim(0);
    }
}

/// No-op fallback on non-glibc targets.
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn trim_malloc() {}

#[cfg(target_os = "linux")]
fn page_size_bytes() -> u64 {
    // SAFETY: sysconf(_SC_PAGESIZE) is always safe; returns -1 on error which we guard.
    let v = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if v > 0 { v as u64 } else { 4096 }
}

#[cfg(not(target_os = "linux"))]
fn page_size_bytes() -> u64 {
    4096
}

/// Spawn a background thread that samples `current_rss_bytes()` every
/// `interval_ms` and updates both `stats.current_rss_bytes` and
/// `stats.peak_rss_bytes` (fetch_max). Exits when `shutdown` is set.
///
/// The sampler is intentionally lightweight — reading `/proc/self/statm` costs
/// ~microseconds and we poll at 500 ms, so overhead is negligible.
pub fn spawn_peak_sampler(
    stats: Arc<StatsTracker>,
    shutdown: Arc<AtomicBool>,
    interval_ms: u64,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("pgmcp-peak-rss".into())
        .spawn(move || {
            while !shutdown.load(Ordering::Acquire) {
                if let Some(rss) = current_rss_bytes() {
                    stats.current_rss_bytes.store(rss, Ordering::Relaxed);
                    stats.peak_rss_bytes.fetch_max(rss, Ordering::Relaxed);
                }
                thread::sleep(Duration::from_millis(interval_ms));
            }
        })
        .expect("spawn peak-rss sampler thread")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_current_rss_is_positive() {
        // On Linux CI this must succeed and return a plausible value.
        #[cfg(target_os = "linux")]
        {
            let rss = current_rss_bytes().expect("statm readable");
            assert!(rss > 0, "RSS must be positive");
            // Sanity bound: less than 1 TB for a test process
            assert!(rss < 1_099_511_627_776u64, "RSS implausibly large: {}", rss);
        }
    }

    #[test]
    fn test_mem_available_is_positive() {
        #[cfg(target_os = "linux")]
        {
            let avail = mem_available_bytes().expect("meminfo readable");
            assert!(avail > 0, "MemAvailable must be positive");
        }
    }
}
