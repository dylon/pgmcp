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
/// safety machinery (dry-run default, `safe_remove` chokepoint, self-project
/// allowlist); we only invoke its public entry point and log the outcome.
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
}
