//! Disk-pressure consumer report — the "what is filling the disk?" breakdown.
//!
//! The `target-cleanup` cron + reaper govern only Rust `target/` + `/tmp`. The
//! biggest consumers on a developer host are usually *outside* that mandate:
//! libvirt VM images, Docker images + build cache, the PostgreSQL database, and
//! the package/toolchain caches (`~/.cache`, `~/.rustup`, `~/.cargo`). When a
//! watched filesystem crosses [`crate::config::DiskGuardConfig::alert_used_pct`]
//! full, the disk watchdog assembles this ranked breakdown so the silent climb
//! becomes an actionable signal — and surfaces it in `/api/status`.
//!
//! It is **read-only** and never deletes anything: the giants it surfaces (VM
//! images, the DB) are the operator's call. It carries a `reclaim_hint` per
//! consumer (the cron that handles it, or the manual command) and, for stale
//! rustup toolchains, the exact `rustup toolchain uninstall` invocations.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, Utc};
use serde::Serialize;
use tracing::debug;

use crate::config::DiskGuardConfig;

/// Recursive-size walk depth bound (consumer dirs are shallow; this guards a
/// pathological tree without following symlinks).
const CONSUMER_WALK_DEPTH: usize = 24;
/// Keep at most this many consumers in the ranked report.
const TOP_CONSUMERS: usize = 12;
/// A dated nightly/beta rustup toolchain older than this (and not active/default)
/// is reported as stale.
const STALE_TOOLCHAIN_DAYS: u64 = 365;

/// One ranked disk consumer.
#[derive(Debug, Clone, Serialize)]
pub struct DiskConsumer {
    pub label: String,
    pub bytes: u64,
    /// How to reclaim it (the responsible cron, or a manual command), if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reclaim_hint: Option<String>,
}

/// The disk-pressure breakdown for one filesystem.
#[derive(Debug, Clone, Serialize)]
pub struct DiskReport {
    pub partition: String,
    pub total_bytes: u64,
    pub avail_bytes: u64,
    pub used_pct: f64,
    /// Top consumers, descending by size.
    pub consumers: Vec<DiskConsumer>,
}

/// Whether to fire the alert this poll: enabled, at/above the used-% threshold,
/// and not already alerting (edge-triggered). Pure — unit-tested.
pub fn alert_should_fire(used_pct: f64, threshold_pct: u8, already: bool) -> bool {
    threshold_pct > 0 && used_pct >= threshold_pct as f64 && !already
}

/// Whether to (re)assemble + store the consumer breakdown on this poll: the
/// alert is enabled and the used-% threshold is crossed, AND it has either never
/// been assembled (`since_last == None`) or the refresh throttle has elapsed.
/// Unlike [`alert_should_fire`] (edge-triggered, drives the once-per-descent log
/// line) this is *level*-triggered on a throttle, so the stored `disk_report`
/// keeps tracking the live disk instead of freezing at the first crossing. Pure
/// — unit-tested.
pub fn report_refresh_due(
    used_pct: f64,
    threshold_pct: u8,
    since_last: Option<Duration>,
    throttle: Duration,
) -> bool {
    threshold_pct > 0
        && used_pct >= threshold_pct as f64
        && since_last.map(|e| e >= throttle).unwrap_or(true)
}

/// Assemble the ranked consumer breakdown for `partition`. `pg_bytes` is the
/// PostgreSQL database size (pre-fetched by the async caller; 0 when unknown).
/// Best-effort throughout: a probe that fails contributes nothing rather than
/// erroring. Blocking (statvfs + dir walks + `docker`/`rustup` shell-outs) — run
/// it via `spawn_blocking`.
pub fn assemble_disk_report(cfg: &DiskGuardConfig, partition: &Path, pg_bytes: u64) -> DiskReport {
    let total = crate::health::fs::fs_total(partition).unwrap_or(0);
    let avail = crate::health::fs::avail_bytes(partition).unwrap_or(0);
    let used_pct = if total > 0 {
        total.saturating_sub(avail) as f64 / total as f64 * 100.0
    } else {
        0.0
    };

    let mut consumers: Vec<DiskConsumer> = Vec::new();

    // Configured directories (libvirt images, ~/.cache, ~/.rustup, ~/.cargo, …).
    for p in &cfg.consumer_paths {
        let path = expand_home(p);
        let bytes = dir_size(&path, CONSUMER_WALK_DEPTH);
        if bytes > 0 {
            consumers.push(DiskConsumer {
                label: p.clone(),
                bytes,
                reclaim_hint: None,
            });
        }
    }

    // Docker (images + build cache) — outside the target-cleanup mandate; the
    // docker-cleanup cron reclaims the build-cache + dangling-image portion.
    if let Some(usage) = crate::cron::docker_cleanup::docker_disk_usage("docker")
        && usage.total_bytes > 0
    {
        consumers.push(DiskConsumer {
            label: "docker (images + build cache)".to_string(),
            bytes: usage.total_bytes,
            reclaim_hint: Some(format!(
                "docker-cleanup reclaims ~{}",
                human_bytes(usage.reclaimable_bytes)
            )),
        });
    }

    // PostgreSQL database (pgmcp's own index).
    if pg_bytes > 0 {
        consumers.push(DiskConsumer {
            label: "postgresql (pgmcp database)".to_string(),
            bytes: pg_bytes,
            reclaim_hint: Some("VACUUM FULL / pg_repack (manual)".to_string()),
        });
    }

    // Stale rustup toolchains (report-only; the operator runs the uninstalls).
    let (stale, stale_bytes, cmds) = stale_toolchain_report();
    if !stale.is_empty() && stale_bytes > 0 {
        consumers.push(DiskConsumer {
            label: format!("rustup stale toolchains ({})", stale.len()),
            bytes: stale_bytes,
            reclaim_hint: Some(cmds.join("; ")),
        });
    }

    consumers.sort_by_key(|c| std::cmp::Reverse(c.bytes));
    consumers.truncate(TOP_CONSUMERS);

    DiskReport {
        partition: partition.display().to_string(),
        total_bytes: total,
        avail_bytes: avail,
        used_pct,
        consumers,
    }
}

/// One-line `label=size [hint], …` rendering of the ranked consumers for the
/// watchdog's alert log. Pure.
pub fn render_consumers(consumers: &[DiskConsumer]) -> String {
    consumers
        .iter()
        .map(|c| match &c.reclaim_hint {
            Some(h) => format!("{}={} [{}]", c.label, human_bytes(c.bytes), h),
            None => format!("{}={}", c.label, human_bytes(c.bytes)),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Human-readable decimal byte size (matches docker's `HumanSize` convention).
fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1000.0 && i < UNITS.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{}{}", b, UNITS[0])
    } else {
        format!("{:.1}{}", v, UNITS[i])
    }
}

/// Recursive size of a path (file length, or the bounded sum of file lengths
/// under a directory). Never follows symlinks. 0 on any error / missing path.
fn dir_size(path: &Path, depth: usize) -> u64 {
    let Ok(md) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    let ft = md.file_type();
    if ft.is_symlink() {
        return 0;
    }
    if ft.is_file() {
        // Allocated size (512-byte blocks), not apparent `len()`, so sparse files
        // — qcow2 VM images especially — are counted at what they actually occupy
        // on disk. Matches `du`; `len()` over-counted libvirt by ~260 GiB.
        return md.blocks().saturating_mul(512);
    }
    if !ft.is_dir() || depth == 0 {
        return 0;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    let mut total = 0u64;
    for e in entries.flatten() {
        total = total.saturating_add(dir_size(&e.path(), depth - 1));
    }
    total
}

/// Expand a leading `~/` to `$HOME`. Other paths pass through unchanged.
fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(p)
}

// ============================================================================
// Stale rustup toolchain report
// ============================================================================

/// One rustup toolchain with its classification + on-disk size.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolchainEntry {
    name: String,
    active: bool,
    default: bool,
    stale: bool,
    bytes: u64,
}

/// Extract the embedded `YYYY-MM-DD` from a dated `nightly-`/`beta-` toolchain
/// name, or `None` for channels without a date (`stable`, `nightly` plain,
/// version pins like `1.74.0-…`). Pure.
fn parse_toolchain_date(name: &str) -> Option<NaiveDate> {
    for prefix in ["nightly-", "beta-"] {
        if let Some(rest) = name.strip_prefix(prefix)
            && let Some(date_part) = rest.get(..10)
            && let Ok(d) = NaiveDate::parse_from_str(date_part, "%Y-%m-%d")
        {
            return Some(d);
        }
    }
    None
}

/// Classify parsed toolchains: a dated nightly/beta older than `stale_days` that
/// is neither active nor default is stale. Pure — unit-tested.
fn classify_toolchains(
    listing: &[(String, bool, bool, u64)],
    now: DateTime<Utc>,
    stale_days: u64,
) -> Vec<ToolchainEntry> {
    let today = now.date_naive();
    listing
        .iter()
        .map(|(name, active, default, bytes)| {
            let stale = !active
                && !default
                && parse_toolchain_date(name)
                    .map(|d| (today - d).num_days() > stale_days as i64)
                    .unwrap_or(false);
            ToolchainEntry {
                name: name.clone(),
                active: *active,
                default: *default,
                stale,
                bytes: *bytes,
            }
        })
        .collect()
}

/// Report stale rustup toolchains: `(entries, total_bytes, uninstall_commands)`.
/// Empty when `rustup` is unavailable (a benign no-op). IO wrapper around
/// [`classify_toolchains`].
fn stale_toolchain_report() -> (Vec<ToolchainEntry>, u64, Vec<String>) {
    let out = match Command::new("rustup")
        .args(["toolchain", "list", "--verbose"])
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => {
            debug!("disk-report: rustup unavailable; skipping toolchain report");
            return (Vec::new(), 0, Vec::new());
        }
    };

    let toolchains_dir = expand_home("~/.rustup/toolchains");
    let listing: Vec<(String, bool, bool, u64)> = out
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let name = line.split_whitespace().next()?.to_string();
            if name.is_empty() {
                return None;
            }
            // Markers live inside the parenthetical (e.g. "(active, default)"),
            // read from there so a path component can't be misread as a marker.
            let (active, default) = match (line.find('('), line.find(')')) {
                (Some(s), Some(e)) if e > s => {
                    let inside = &line[s + 1..e];
                    (inside.contains("active"), inside.contains("default"))
                }
                _ => (false, false),
            };
            let bytes = dir_size(&toolchains_dir.join(&name), CONSUMER_WALK_DEPTH);
            Some((name, active, default, bytes))
        })
        .collect();

    let entries = classify_toolchains(&listing, Utc::now(), STALE_TOOLCHAIN_DAYS);
    let stale: Vec<ToolchainEntry> = entries.into_iter().filter(|e| e.stale).collect();
    let bytes = stale.iter().map(|e| e.bytes).sum();
    let cmds = stale
        .iter()
        .map(|e| format!("rustup toolchain uninstall {}", e.name))
        .collect();
    (stale, bytes, cmds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let p =
                std::env::temp_dir().join(format!("pgmcp-dr-test-{}-{}", std::process::id(), seq));
            std::fs::create_dir_all(&p).expect("mkdir");
            TempDir(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn alert_edges() {
        assert!(alert_should_fire(86.0, 85, false));
        assert!(alert_should_fire(85.0, 85, false), "boundary is inclusive");
        assert!(!alert_should_fire(86.0, 85, true), "already alerting");
        assert!(!alert_should_fire(80.0, 85, false), "below threshold");
        assert!(!alert_should_fire(99.0, 0, false), "0 disables the alert");
    }

    #[test]
    fn report_refresh_throttle() {
        let thr = Duration::from_secs(300);
        // Above threshold, never assembled → assemble now.
        assert!(report_refresh_due(96.0, 85, None, thr));
        // Above threshold, throttle elapsed → re-assemble (the staleness fix:
        // a later above-threshold poll past the throttle DOES refresh).
        assert!(report_refresh_due(
            96.0,
            85,
            Some(Duration::from_secs(301)),
            thr
        ));
        // Above threshold but within the throttle window → keep the stored report.
        assert!(!report_refresh_due(
            96.0,
            85,
            Some(Duration::from_secs(120)),
            thr
        ));
        // Below threshold → never assemble (regardless of elapsed).
        assert!(!report_refresh_due(80.0, 85, None, thr));
        // Disabled (0) → never assemble.
        assert!(!report_refresh_due(99.0, 0, None, thr));
        // Boundary is inclusive on both axes.
        assert!(report_refresh_due(85.0, 85, Some(thr), thr));
    }

    #[test]
    fn human_bytes_scales() {
        assert_eq!(human_bytes(0), "0B");
        assert_eq!(human_bytes(512), "512B");
        assert_eq!(human_bytes(1_500), "1.5KB");
        assert_eq!(human_bytes(2_000_000_000), "2.0GB");
    }

    #[test]
    fn dir_size_sums_files_skips_symlinks() {
        let tmp = TempDir::new();
        std::fs::write(tmp.path().join("a"), vec![0u8; 100]).unwrap();
        std::fs::create_dir_all(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("sub/b"), vec![0u8; 50]).unwrap();
        // `dir_size` reports *allocated* size (blocks×512) — fs-block aligned and
        // ≥ the apparent byte length — so derive the expectation from the same
        // metadata rather than the bytes written.
        let alloc = |p: &Path| {
            std::fs::symlink_metadata(p)
                .map(|m| m.blocks() * 512)
                .unwrap_or(0)
        };
        let a = tmp.path().join("a");
        let b = tmp.path().join("sub/b");
        assert_eq!(dir_size(tmp.path(), 8), alloc(&a) + alloc(&b));
        assert_eq!(dir_size(&a, 8), alloc(&a), "single file");
        assert!(
            alloc(&a) >= 100,
            "allocated size is at least the apparent size"
        );
        assert_eq!(dir_size(Path::new("/pgmcp/no/such"), 8), 0);
    }

    #[test]
    fn parse_toolchain_date_only_dated_channels() {
        assert_eq!(
            parse_toolchain_date("nightly-2022-08-08-x86_64-unknown-linux-gnu"),
            NaiveDate::from_ymd_opt(2022, 8, 8)
        );
        assert_eq!(
            parse_toolchain_date("beta-2025-01-01-x86_64-unknown-linux-gnu"),
            NaiveDate::from_ymd_opt(2025, 1, 1)
        );
        assert_eq!(
            parse_toolchain_date("stable-x86_64-unknown-linux-gnu"),
            None
        );
        assert_eq!(
            parse_toolchain_date("nightly-x86_64-unknown-linux-gnu"),
            None
        );
        assert_eq!(
            parse_toolchain_date("1.74.0-x86_64-unknown-linux-gnu"),
            None
        );
    }

    #[test]
    fn classify_toolchains_flags_only_old_unpinned() {
        let now = Utc.with_ymd_and_hms(2026, 6, 25, 0, 0, 0).unwrap();
        let listing = vec![
            (
                "nightly-2022-08-08-x86_64-unknown-linux-gnu".to_string(),
                false,
                false,
                2_000_000_000,
            ),
            (
                "stable-x86_64-unknown-linux-gnu".to_string(),
                true,
                true,
                1_300_000_000,
            ),
            (
                "nightly-2026-06-15-x86_64-unknown-linux-gnu".to_string(),
                false,
                false,
                1_300_000_000,
            ),
            (
                "nightly-x86_64-unknown-linux-gnu".to_string(),
                false,
                true,
                2_000_000_000,
            ),
            (
                "1.74.0-x86_64-unknown-linux-gnu".to_string(),
                false,
                false,
                1_000_000_000,
            ),
        ];
        let out = classify_toolchains(&listing, now, 365);
        let stale: Vec<&str> = out
            .iter()
            .filter(|e| e.stale)
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(stale, vec!["nightly-2022-08-08-x86_64-unknown-linux-gnu"]);
    }

    #[test]
    fn assemble_report_ranks_and_labels() {
        let tmp = TempDir::new();
        let big = tmp.path().join("big");
        std::fs::write(&big, vec![0u8; 4096]).unwrap();
        let cfg = DiskGuardConfig {
            consumer_paths: vec![big.to_string_lossy().into_owned()],
            ..Default::default()
        };
        let report = assemble_disk_report(&cfg, Path::new("/"), 9_000_000_000);

        // Partition leg (real `/`) populates sanely.
        assert!(report.total_bytes > 0);
        assert!(report.used_pct >= 0.0 && report.used_pct <= 100.0);
        // The configured consumer + the PostgreSQL consumer are present.
        assert!(
            report.consumers.iter().any(|c| c.bytes == 4096),
            "configured dir consumer present"
        );
        let pg = report
            .consumers
            .iter()
            .find(|c| c.label.starts_with("postgresql"))
            .expect("pg consumer present when pg_bytes > 0");
        assert_eq!(pg.bytes, 9_000_000_000);
        // Descending by size.
        for w in report.consumers.windows(2) {
            assert!(w[0].bytes >= w[1].bytes, "consumers sorted descending");
        }
        // Rendering is non-empty and includes a hint marker for pg.
        let rendered = render_consumers(&report.consumers);
        assert!(rendered.contains("postgresql"));
        assert!(rendered.contains('['), "hints rendered in brackets");
    }
}
