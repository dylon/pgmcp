//! Disk-space watchdog — pressure-driven, **complements** the `target-cleanup`
//! cron (it adds no deletion logic; it reuses the cron's entry point).
//!
//! `target-cleanup` reclaims disk on a long interval (default 7 days). The
//! watchdog watches free space *continuously* and, when a filesystem crosses a
//! pause floor on **bytes or inodes** (a disk can ENOSPC on either), it (a) sets
//! the shared [`DiskPressure`] flag so pgmcp pauses its own disk-growing work
//! (indexing + heavy crons) and (b) triggers the cron out-of-band so reclamation
//! happens *now*. Hysteresis (resume floor > pause floor) prevents flapping.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use sqlx::PgPool;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::health::fs::{FsAvail, fs_avail};
use crate::stats::tracker::StatsTracker;

const GIB: u64 = 1 << 30;

/// What the pure threshold logic decides for one poll. Separated from IO so it
/// is exhaustively table-testable without a filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Above all floors (or in the dead-band while paused) — do nothing.
    None,
    /// Below a warn floor but not yet a pause floor — log an early warning.
    Warn,
    /// Crossed a pause floor while not paused — enter pressure.
    EnterPressure,
    /// Recovered above the resume floors while paused — exit pressure.
    ExitPressure,
}

/// Already-resolved (clamped, byte-and-inode) thresholds for [`decide`].
#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    pub warn_bytes: u64,
    pub pause_bytes: u64,
    pub resume_bytes: u64,
    pub warn_inodes: u64,
    pub pause_inodes: u64,
    pub resume_inodes: u64,
}

/// Pure hysteresis decision. A `0` floor disables that axis. **Enter** triggers
/// if *either* axis is below its pause floor; **exit** requires *both* axes back
/// above their resume floors (never resume while still inode-starved).
pub fn decide(avail: FsAvail, paused: bool, t: &Thresholds) -> Decision {
    let below = |v: u64, floor: u64| floor > 0 && v < floor;
    if paused {
        let bytes_ok = t.resume_bytes == 0 || avail.avail_bytes >= t.resume_bytes;
        let inodes_ok = t.resume_inodes == 0 || avail.avail_inodes >= t.resume_inodes;
        if bytes_ok && inodes_ok {
            Decision::ExitPressure
        } else {
            Decision::None
        }
    } else if below(avail.avail_bytes, t.pause_bytes) || below(avail.avail_inodes, t.pause_inodes) {
        Decision::EnterPressure
    } else if below(avail.avail_bytes, t.warn_bytes) || below(avail.avail_inodes, t.warn_inodes) {
        Decision::Warn
    } else {
        Decision::None
    }
}

// ============================================================================
// Memory watchdog — the RAM analogue of the disk watchdog above. Reuses the
// `Decision` enum. Two axes of OPPOSITE polarity: system available RAM (low is
// bad, like the disk floors) and process RSS vs. its budget (high is bad).
// ============================================================================

/// Resolved memory thresholds for [`mem_decide`], in bytes. A `0` floor disables
/// that axis. The available-RAM axis is low-is-bad (`pause_avail` < `resume_avail`);
/// the RSS axis is high-is-bad (`resume_rss` < `pause_rss`).
#[derive(Debug, Clone, Copy)]
pub struct MemThresholds {
    pub warn_avail: u64,
    pub pause_avail: u64,
    pub resume_avail: u64,
    pub pause_rss: u64,
    pub resume_rss: u64,
}

/// Pure hysteresis decision for the memory watchdog. **Enter** if *either* axis
/// trips its pause floor (available RAM too low, or process RSS too high);
/// **exit** only when *both* axes have recovered past their resume floors (never
/// resume while one axis is still under pressure). A `0` floor disables its axis.
pub fn mem_decide(avail: u64, rss: u64, paused: bool, t: &MemThresholds) -> Decision {
    let avail_low = t.pause_avail > 0 && avail < t.pause_avail;
    let rss_high = t.pause_rss > 0 && rss > t.pause_rss;
    if paused {
        let avail_ok = t.resume_avail == 0 || avail >= t.resume_avail;
        let rss_ok = t.resume_rss == 0 || rss <= t.resume_rss;
        if avail_ok && rss_ok {
            Decision::ExitPressure
        } else {
            Decision::None
        }
    } else if avail_low || rss_high {
        Decision::EnterPressure
    } else if t.warn_avail > 0 && avail < t.warn_avail {
        Decision::Warn
    } else {
        Decision::None
    }
}

/// Spawn the memory-watchdog loop. Skipped by the caller when the guard is
/// disabled (`pause_avail_mib == 0 && pause_rss_mib == 0`). Polls system
/// available RAM + process RSS every `poll_interval_secs`; on the pause edge it
/// sets the shared [`MemoryPressure`](crate::health::MemoryPressure) flag (so
/// heavy crons + the embed/ingest intake pause) and calls `malloc_trim(0)` to
/// hand retained glibc-arena high-water back to the kernel. The RSS budget is
/// resolved **once** at start so the ceiling doesn't drift with live free RAM.
pub fn spawn_memory_watchdog(
    stats: Arc<StatsTracker>,
    config: Arc<ArcSwap<Config>>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        {
            let mg = config.load();
            info!(
                pause_rss_mib = mg.mem_guard.pause_rss_mib,
                pause_avail_mib = mg.mem_guard.pause_avail_mib,
                "memory-watchdog: started"
            );
        }
        let mut warned = false;
        loop {
            let mg = config.load().mem_guard.clone();
            let interval = Duration::from_secs(mg.poll_interval_secs.max(1));
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(interval) => {}
            }

            let Some(avail) = crate::stats::rss::mem_available_bytes() else {
                continue;
            };
            let Some(rss) = crate::stats::rss::current_rss_bytes() else {
                continue;
            };
            stats.memory_pressure().record(avail, rss);

            // Periodic arena reclaim: hand retained glibc-arena high-water back to
            // the kernel whenever RSS is elevated, REGARDLESS of source. The
            // per-heavy-cron trim (`scheduler.rs`) covers only the 18 heavy crons;
            // it misses the many NON-heavy recorded crons (`project-deps-index`
            // +2.7 GB/run, `target-cleanup` +2 GB/run, `mcp-client-liveness`,
            // `stale-cleanup`, …) and the request path, whose large transient
            // allocations otherwise accumulate untrimmed into a tens-of-GB balloon
            // over hours (2026-07-08 incident: 50 GB over 10 h, all anonymous heap).
            // A poll-cadence trim above `trim_above_rss_mib` keeps RSS bounded near
            // that floor — well below the pause / `MemoryHigh` ceilings — so the
            // balloon can never build. Off the reactor (the trim walks arena locks
            // for tens of ms on a large heap). `0` disables.
            let trim_floor = mg.trim_above_rss_mib << 20;
            if trim_floor > 0 && rss >= trim_floor {
                tokio::task::spawn_blocking(crate::stats::rss::trim_malloc);
            }

            // Absolute RSS floors (MiB → bytes); both 0 if the axis is disabled.
            let pause_rss = mg.pause_rss_mib << 20;
            let resume_rss = if pause_rss > 0 {
                // resume must be strictly BELOW pause (high-is-bad axis).
                (mg.resume_rss_mib << 20).min(pause_rss.saturating_sub(1))
            } else {
                0
            };
            let pause_avail = mg.pause_avail_mib << 20;
            let resume_avail = if pause_avail > 0 {
                // resume must be strictly ABOVE pause (low-is-bad axis).
                (mg.resume_avail_mib << 20).max(pause_avail + 1)
            } else {
                0
            };
            let t = MemThresholds {
                warn_avail: mg.warn_avail_mib << 20,
                pause_avail,
                resume_avail,
                pause_rss,
                resume_rss,
            };

            match mem_decide(avail, rss, stats.memory_pressure().is_paused(), &t) {
                Decision::Warn => {
                    if !warned {
                        warned = true;
                        warn!(
                            avail_mib = avail >> 20,
                            rss_mib = rss >> 20,
                            "memory-watchdog: available RAM low (warn floor)"
                        );
                    }
                }
                Decision::EnterPressure => {
                    if stats.memory_pressure().enter_pressure() {
                        warned = true;
                        warn!(
                            avail_mib = avail >> 20,
                            rss_mib = rss >> 20,
                            pause_avail_mib = mg.pause_avail_mib,
                            pause_rss_mib = pause_rss >> 20,
                            "memory-watchdog: entering pressure — pausing heavy crons + indexing, trimming heap"
                        );
                        // Return retained glibc-arena high-water to the kernel.
                        // Off the reactor (the trim walks arena locks for tens of
                        // ms on a large heap).
                        tokio::task::spawn_blocking(crate::stats::rss::trim_malloc);
                    }
                }
                Decision::ExitPressure => {
                    if stats.memory_pressure().exit_pressure() {
                        warned = false;
                        info!(
                            avail_mib = avail >> 20,
                            rss_mib = rss >> 20,
                            "memory-watchdog: recovered — resuming heavy crons + indexing"
                        );
                    }
                }
                Decision::None => {}
            }
        }
        info!("memory-watchdog: stopped");
    })
}

/// Spawn the watchdog loop. Skipped by the caller when the guard is disabled
/// (`pause_floor_gb == 0`).
pub fn spawn_disk_watchdog(
    pool: PgPool,
    stats: Arc<StatsTracker>,
    config: Arc<ArcSwap<Config>>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        info!("disk-watchdog: started");
        // Edge flags so each line fires once per descent, not every poll.
        let mut warned = false;
        let mut alerted = false;
        // When the (expensive) consumer breakdown was last assembled — throttles
        // its refresh while we stay above the alert threshold (the live used-% is
        // recorded every poll regardless).
        let mut last_report_at: Option<Instant> = None;
        loop {
            let dg = config.load().disk_guard.clone();
            let interval = Duration::from_secs(dg.poll_interval_secs.max(5));
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(interval) => {}
            }

            let paths = watch_paths(&config.load());
            let Some(avail) = min_avail(&paths) else {
                continue;
            };
            stats.disk_pressure().record_avail(avail.avail_bytes);

            // Resolve thresholds; clamp resume strictly above pause to forbid a
            // zero-width hysteresis band (which would flap).
            let pause_bytes = dg.pause_floor_gb.saturating_mul(GIB);
            let resume_bytes = dg
                .resume_floor_gb
                .saturating_mul(GIB)
                .max(pause_bytes.saturating_add(1));
            let resume_inodes = if dg.pause_floor_inodes == 0 {
                dg.resume_floor_inodes
            } else {
                dg.resume_floor_inodes.max(dg.pause_floor_inodes + 1)
            };
            let t = Thresholds {
                warn_bytes: dg.warn_floor_gb.saturating_mul(GIB),
                pause_bytes,
                resume_bytes,
                warn_inodes: dg.warn_floor_inodes,
                pause_inodes: dg.pause_floor_inodes,
                resume_inodes,
            };

            match decide(avail, stats.disk_pressure().is_paused(), &t) {
                Decision::Warn => {
                    if !warned {
                        warned = true;
                        warn!(
                            avail_gb = avail.avail_bytes / GIB,
                            avail_inodes = avail.avail_inodes,
                            "disk-watchdog: free space low (warn floor)"
                        );
                    }
                }
                Decision::EnterPressure => {
                    if stats.disk_pressure().enter_pressure() {
                        warned = true;
                        warn!(
                            avail_gb = avail.avail_bytes / GIB,
                            avail_inodes = avail.avail_inodes,
                            pause_floor_gb = dg.pause_floor_gb,
                            pause_floor_inodes = dg.pause_floor_inodes,
                            "disk-watchdog: entering pressure — pausing pgmcp writes + triggering cleanup"
                        );
                        run_complementary_cleanup(&pool, &config).await;
                    }
                }
                Decision::ExitPressure => {
                    if stats.disk_pressure().exit_pressure() {
                        warned = false;
                        info!(
                            avail_gb = avail.avail_bytes / GIB,
                            avail_inodes = avail.avail_inodes,
                            "disk-watchdog: recovered — resuming pgmcp writes"
                        );
                    }
                }
                Decision::None => {}
            }

            // Disk-pressure alert + live fullness. `worst_used_pct` is cheap
            // (statvfs only), so it is recorded every poll as the `/api/status`
            // headline that always matches `df`. The ranked-consumer breakdown is
            // expensive (dir-walks + `docker`/`rustup` shell-outs), so while a
            // filesystem stays at/above `alert_used_pct` (a softer, earlier signal
            // than the byte/inode pause floors — a 2 TB disk can be 95% full with
            // 100+ GiB free, tripping no floor) it is re-assembled only on a
            // throttle — but *continuously*, so the stored report tracks the live
            // disk instead of freezing at the first crossing. The alert *log line*
            // stays edge-triggered (once per descent).
            if let Some((part, used_pct)) = worst_used_pct(&paths) {
                stats.set_disk_used_pct(used_pct);
                let throttle = Duration::from_secs(dg.report_refresh_secs.max(30));
                let since_last = last_report_at.map(|t| t.elapsed());
                if crate::health::disk_report::report_refresh_due(
                    used_pct,
                    dg.alert_used_pct,
                    since_last,
                    throttle,
                ) {
                    last_report_at = Some(Instant::now());
                    let pg_bytes = pg_database_size(&pool).await.unwrap_or(0);
                    let dg2 = dg.clone();
                    let part2 = part.clone();
                    if let Ok(report) = tokio::task::spawn_blocking(move || {
                        crate::health::disk_report::assemble_disk_report(&dg2, &part2, pg_bytes)
                    })
                    .await
                    {
                        // Edge-triggered log line (once per descent); the stored
                        // report refreshes every throttle interval regardless.
                        if crate::health::disk_report::alert_should_fire(
                            used_pct,
                            dg.alert_used_pct,
                            alerted,
                        ) {
                            alerted = true;
                            warn!(
                                used_pct = format!("{:.1}", report.used_pct),
                                partition = %report.partition,
                                avail_gb = report.avail_bytes / GIB,
                                top_consumers = %crate::health::disk_report::render_consumers(&report.consumers),
                                "disk-watchdog: disk pressure — top reclaimable consumers"
                            );
                        }
                        stats.set_disk_report(report);
                    }
                } else if dg.alert_used_pct > 0 && used_pct < dg.alert_used_pct as f64 {
                    alerted = false; // recovered below the alert threshold
                    last_report_at = None;
                }
            }
        }
        info!("disk-watchdog: stopped");
    })
}

/// Out-of-band trigger of the existing `target-cleanup` cron. Reuses ALL of its
/// safety machinery (dry-run default, `safe_remove` chokepoint, `/proc`
/// busy-scan); we only invoke its public entry point and log the outcome.
async fn run_complementary_cleanup(pool: &PgPool, config: &Arc<ArcSwap<Config>>) {
    let tc = config.load().cron.target_cleanup.clone();
    let report = crate::cron::target_cleanup::run_target_cleanup(pool, &tc, None).await;
    let reclaimed = report.total_bytes();
    if tc.dry_run {
        warn!(
            would_reclaim_bytes = reclaimed,
            "disk-watchdog: out-of-band cleanup ran in DRY-RUN — set [cron.target_cleanup] dry_run=false to reclaim automatically"
        );
    } else {
        info!(
            reclaimed_bytes = reclaimed,
            "disk-watchdog: out-of-band cleanup reclaimed"
        );
    }
}

/// Resolve which filesystems to watch: explicit `[disk_guard] paths`, else the
/// cleanup roots, else the workspace paths, else `/`.
fn watch_paths(cfg: &Config) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = if !cfg.disk_guard.paths.is_empty() {
        cfg.disk_guard.paths.iter().map(PathBuf::from).collect()
    } else if !cfg.cron.target_cleanup.roots.is_empty() {
        cfg.cron
            .target_cleanup
            .roots
            .iter()
            .map(PathBuf::from)
            .collect()
    } else if !cfg.workspace.paths.is_empty() {
        cfg.workspace.paths.iter().map(PathBuf::from).collect()
    } else {
        Vec::new()
    };
    if v.is_empty() {
        v.push(PathBuf::from("/"));
    }
    v
}

/// Worst-case (minimum) availability across all watched filesystems.
fn min_avail(paths: &[PathBuf]) -> Option<FsAvail> {
    paths
        .iter()
        .filter_map(|p| fs_avail(p))
        .reduce(FsAvail::min)
}

/// The watched filesystem with the highest used-percentage, paired with that
/// percentage — the partition the disk-pressure alert reports on.
fn worst_used_pct(paths: &[PathBuf]) -> Option<(PathBuf, f64)> {
    paths
        .iter()
        .filter_map(|p| {
            let total = crate::health::fs::fs_total(p)?;
            if total == 0 {
                return None;
            }
            let avail = crate::health::fs::avail_bytes(p)?;
            let used_pct = total.saturating_sub(avail) as f64 / total as f64 * 100.0;
            Some((p.clone(), used_pct))
        })
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
}

/// Best-effort PostgreSQL database size for the disk-pressure report.
async fn pg_database_size(pool: &PgPool) -> Option<u64> {
    match sqlx::query_scalar::<_, i64>("SELECT pg_database_size(current_database())")
        .fetch_one(pool)
        .await
    {
        Ok(v) => Some(v.max(0) as u64),
        Err(e) => {
            error!(error = %e, "disk-watchdog: pg_database_size query failed");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> Thresholds {
        // 20 GiB warn, 10 GiB pause, 25 GiB resume; inodes 2M/1M/3M.
        Thresholds {
            warn_bytes: 20 * GIB,
            pause_bytes: 10 * GIB,
            resume_bytes: 25 * GIB,
            warn_inodes: 2_000_000,
            pause_inodes: 1_000_000,
            resume_inodes: 3_000_000,
        }
    }
    fn avail(gb: u64, inodes: u64) -> FsAvail {
        FsAvail {
            avail_bytes: gb * GIB,
            avail_inodes: inodes,
        }
    }

    #[test]
    fn above_all_floors_does_nothing() {
        assert_eq!(decide(avail(30, 5_000_000), false, &t()), Decision::None);
    }

    #[test]
    fn warn_band_on_bytes() {
        assert_eq!(decide(avail(15, 5_000_000), false, &t()), Decision::Warn);
    }

    #[test]
    fn enter_on_low_bytes() {
        assert_eq!(
            decide(avail(8, 5_000_000), false, &t()),
            Decision::EnterPressure
        );
    }

    #[test]
    fn enter_on_low_inodes_even_with_bytes_free() {
        // Plenty of bytes, but inodes below the pause floor → still enter.
        assert_eq!(
            decide(avail(500, 900_000), false, &t()),
            Decision::EnterPressure
        );
    }

    #[test]
    fn warn_band_on_inodes() {
        assert_eq!(decide(avail(500, 1_500_000), false, &t()), Decision::Warn);
    }

    #[test]
    fn dead_band_while_paused_does_not_resume() {
        // Between pause (10G) and resume (25G): must NOT resume.
        assert_eq!(decide(avail(20, 5_000_000), true, &t()), Decision::None);
    }

    #[test]
    fn resume_requires_both_axes() {
        // Bytes recovered but inodes still under resume → stay paused.
        assert_eq!(decide(avail(30, 2_000_000), true, &t()), Decision::None);
        // Both recovered → exit.
        assert_eq!(
            decide(avail(30, 3_500_000), true, &t()),
            Decision::ExitPressure
        );
    }

    #[test]
    fn disabled_axis_is_ignored() {
        let mut th = t();
        th.pause_inodes = 0;
        th.warn_inodes = 0;
        th.resume_inodes = 0;
        // Inode axis disabled: 0 free inodes must not trip anything by inodes.
        assert_eq!(decide(avail(30, 0), false, &th), Decision::None);
    }

    // ---- memory watchdog (mem_decide) ----

    fn mt() -> MemThresholds {
        // avail: warn 12 GiB, pause 8 GiB, resume 16 GiB (low-is-bad).
        // rss:   pause 40 GiB, resume 28 GiB (high-is-bad).
        MemThresholds {
            warn_avail: 12 * GIB,
            pause_avail: 8 * GIB,
            resume_avail: 16 * GIB,
            pause_rss: 40 * GIB,
            resume_rss: 28 * GIB,
        }
    }

    #[test]
    fn mem_above_all_floors_is_none() {
        assert_eq!(mem_decide(20 * GIB, 10 * GIB, false, &mt()), Decision::None);
    }

    #[test]
    fn mem_warn_band_on_low_avail() {
        // 10 GiB free: below the 12 warn floor, above the 8 pause floor.
        assert_eq!(mem_decide(10 * GIB, 10 * GIB, false, &mt()), Decision::Warn);
    }

    #[test]
    fn mem_enter_on_low_avail() {
        assert_eq!(
            mem_decide(6 * GIB, 10 * GIB, false, &mt()),
            Decision::EnterPressure
        );
    }

    #[test]
    fn mem_enter_on_high_rss_even_with_avail_free() {
        // Plenty of free RAM, but this process's RSS exceeds its ceiling → enter.
        assert_eq!(
            mem_decide(50 * GIB, 45 * GIB, false, &mt()),
            Decision::EnterPressure
        );
    }

    #[test]
    fn mem_dead_band_while_paused_does_not_resume() {
        // RSS between resume (28) and pause (40), avail fine → hold pressure.
        assert_eq!(mem_decide(20 * GIB, 33 * GIB, true, &mt()), Decision::None);
    }

    #[test]
    fn mem_resume_requires_both_axes() {
        // avail recovered (>16) but RSS still above resume (28) → stay paused.
        assert_eq!(mem_decide(20 * GIB, 30 * GIB, true, &mt()), Decision::None);
        // RSS fine but avail still under resume (<16) → stay paused.
        assert_eq!(mem_decide(12 * GIB, 10 * GIB, true, &mt()), Decision::None);
        // Both axes recovered → exit.
        assert_eq!(
            mem_decide(20 * GIB, 27 * GIB, true, &mt()),
            Decision::ExitPressure
        );
    }

    #[test]
    fn mem_disabled_rss_axis_is_ignored() {
        let mut th = mt();
        th.pause_rss = 0;
        th.resume_rss = 0;
        // RSS axis off: even a huge RSS must not trip it; only avail matters.
        assert_eq!(mem_decide(20 * GIB, 200 * GIB, false, &th), Decision::None);
    }

    #[test]
    fn mem_disabled_avail_axis_is_ignored() {
        let mut th = mt();
        th.warn_avail = 0;
        th.pause_avail = 0;
        th.resume_avail = 0;
        // avail axis off: 0 free must not trip; only RSS matters.
        assert_eq!(mem_decide(0, 10 * GIB, false, &th), Decision::None);
        assert_eq!(mem_decide(0, 45 * GIB, false, &th), Decision::EnterPressure);
    }
}
