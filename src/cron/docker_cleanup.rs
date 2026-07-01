//! `docker-cleanup` cron — bounded, safe reclamation of Docker disk usage.
//!
//! Docker's build cache and dangling (untagged) images grow without bound on a
//! developer host that builds images regularly (e.g. running Kroki + companion
//! containers). This cron reclaims **only** those two regeneratable classes:
//!
//!   * **Build cache** — `docker builder prune` (optionally age-filtered so an
//!     in-flight build's fresh cache is kept).
//!   * **Dangling images** — `docker image prune --filter dangling=true`
//!     (untagged layers no tagged image references).
//!
//! It **never** touches tagged images, running containers, or named volumes, so
//! it cannot break a running stack. Like `target-cleanup`, it ships **enabled
//! but `dry_run = true`**: in dry-run it reports the reclaimable size (read from
//! `docker system df`) and prunes nothing until an operator sets
//! `dry_run = false`. If `docker` is not installed or its daemon is unreachable
//! the run is a quiet no-op (`available = false`).
//!
//! Config: [`crate::config::DockerCleanupConfig`] (`[cron.docker_cleanup]`).

use std::process::Command;

use tracing::{debug, error, info};

use crate::config::DockerCleanupConfig;

/// Daemon-facing entry point: run one bounded prune and log the summary,
/// off-loading the blocking `docker` calls from the async runtime. Called from
/// the scheduler on the configured interval.
pub async fn run_or_log(cfg: DockerCleanupConfig) -> DockerReport {
    let report = match tokio::task::spawn_blocking(move || run_docker_cleanup(&cfg)).await {
        Ok(r) => r,
        Err(e) => {
            error!(error = %e, "docker-cleanup: blocking pass panicked");
            DockerReport::default()
        }
    };
    report.log_summary();
    report
}

/// Outcome of one docker-cleanup pass.
#[derive(Debug, Default, Clone)]
pub struct DockerReport {
    /// Whether the docker CLI + daemon were reachable this run.
    pub available: bool,
    pub dry_run: bool,
    /// Build-cache bytes reclaimed (or, under dry-run, reclaimable).
    pub builder_reclaimed_bytes: u64,
    /// Dangling-image bytes reclaimed (or, under dry-run, reclaimable).
    pub images_reclaimed_bytes: u64,
    pub errors: u64,
}

impl DockerReport {
    pub fn total_bytes(&self) -> u64 {
        self.builder_reclaimed_bytes + self.images_reclaimed_bytes
    }

    /// Render this pass's reclaimed-byte counts as a `cron_run_history.counters`
    /// JSON object, so `cron_history` reflects what docker-cleanup reclaimed.
    pub fn to_counters(&self) -> serde_json::Value {
        serde_json::json!({
            "available": self.available,
            "dry_run": self.dry_run,
            "builder_reclaimed_bytes": self.builder_reclaimed_bytes,
            "images_reclaimed_bytes": self.images_reclaimed_bytes,
            "total_bytes": self.total_bytes(),
            "errors": self.errors,
        })
    }

    /// Emit the single summary line (the `RECLAIMED …` record).
    pub fn log_summary(&self) {
        info!(
            available = self.available,
            dry_run = self.dry_run,
            builder_reclaimed_bytes = self.builder_reclaimed_bytes,
            images_reclaimed_bytes = self.images_reclaimed_bytes,
            total_bytes = self.total_bytes(),
            errors = self.errors,
            "docker-cleanup RECLAIMED"
        );
    }
}

/// The synchronous heart of the cron. Pure of async; safe to `spawn_blocking`.
fn run_docker_cleanup(cfg: &DockerCleanupConfig) -> DockerReport {
    let mut report = DockerReport {
        dry_run: cfg.dry_run,
        ..Default::default()
    };

    // Availability: docker CLI present AND daemon reachable (server version).
    if !docker_available(cfg) {
        debug!(docker_bin = %cfg.docker_bin, "docker-cleanup: docker unavailable; skipping");
        return report; // available stays false — a quiet no-op
    }
    report.available = true;

    if cfg.dry_run {
        // Report the reclaimable size without pruning anything.
        match run_docker(cfg, &["system", "df", "--format", "{{json .}}"]) {
            Ok(out) => {
                let (build, images) = parse_system_df_reclaimable(&out);
                report.builder_reclaimed_bytes = build;
                report.images_reclaimed_bytes = if cfg.prune_dangling_images { images } else { 0 };
            }
            Err(e) => {
                error!(error = %e, "docker-cleanup: `docker system df` failed");
                report.errors += 1;
            }
        }
        return report;
    }

    // Armed: prune the build cache (optionally age-filtered).
    let until = format!("until={}h", cfg.builder_until_hours);
    let mut builder_args: Vec<&str> = vec!["builder", "prune", "--force"];
    if cfg.builder_until_hours > 0 {
        builder_args.push("--filter");
        builder_args.push(&until);
    }
    match run_docker(cfg, &builder_args) {
        Ok(out) => report.builder_reclaimed_bytes = parse_reclaimed(&out),
        Err(e) => {
            error!(error = %e, "docker-cleanup: builder prune failed");
            report.errors += 1;
        }
    }

    // Prune dangling (untagged) images.
    if cfg.prune_dangling_images {
        match run_docker(
            cfg,
            &["image", "prune", "--force", "--filter", "dangling=true"],
        ) {
            Ok(out) => report.images_reclaimed_bytes = parse_reclaimed(&out),
            Err(e) => {
                error!(error = %e, "docker-cleanup: image prune failed");
                report.errors += 1;
            }
        }
    }

    report
}

/// True when the docker CLI is present and its daemon answers a version query.
fn docker_available(cfg: &DockerCleanupConfig) -> bool {
    run_docker(cfg, &["version", "--format", "{{.Server.Version}}"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

/// Run `<docker_bin> <args>`; `Ok(stdout)` on exit-0, `Err(reason)` on a spawn
/// error or non-zero exit (reason carries stderr for the error log).
fn run_docker(cfg: &DockerCleanupConfig, args: &[&str]) -> Result<String, String> {
    let out = Command::new(&cfg.docker_bin)
        .args(args)
        .output()
        .map_err(|e| format!("spawn failed: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "exit {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Parse a docker `HumanSize` string (e.g. `"1.5GB"`, `"512kB"`, `"0B"`, or a
/// binary `"2GiB"`) into bytes. Docker prints decimal (1000-based) units;
/// `*iB` suffixes are handled as binary for robustness. Pure — unit-tested.
fn parse_human_size(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let split = s.find(|c: char| c.is_ascii_alphabetic());
    let (num, unit) = match split {
        Some(i) => (s[..i].trim(), s[i..].trim()),
        None => (s, ""),
    };
    let val: f64 = num.parse().ok()?;
    let mult: f64 = match unit.to_ascii_uppercase().as_str() {
        "" | "B" => 1.0,
        "KB" | "K" => 1e3,
        "MB" | "M" => 1e6,
        "GB" | "G" => 1e9,
        "TB" | "T" => 1e12,
        "PB" | "P" => 1e15,
        "KIB" => 1024.0,
        "MIB" => 1024_f64.powi(2),
        "GIB" => 1024_f64.powi(3),
        "TIB" => 1024_f64.powi(4),
        "PIB" => 1024_f64.powi(5),
        _ => return None,
    };
    Some((val * mult) as u64)
}

/// Extract the bytes from a `docker {builder,image} prune` stdout's
/// `"Total reclaimed space: <size>"` line (0 if absent). Pure — unit-tested.
fn parse_reclaimed(stdout: &str) -> u64 {
    for line in stdout.lines() {
        if let Some(rest) = line.trim().strip_prefix("Total reclaimed space:") {
            return parse_human_size(rest).unwrap_or(0);
        }
    }
    0
}

/// Total disk footprint + cleanup-reclaimable bytes reported by Docker.
pub(crate) struct DockerUsage {
    pub total_bytes: u64,
    pub reclaimable_bytes: u64,
}

/// Probe Docker's disk usage for the disk-pressure report (best-effort; `None`
/// if docker is unavailable). `total` sums every resource type's `Size`;
/// `reclaimable` is the build-cache + dangling-image portion this cron prunes.
pub(crate) fn docker_disk_usage(docker_bin: &str) -> Option<DockerUsage> {
    let cfg = DockerCleanupConfig {
        docker_bin: docker_bin.to_string(),
        ..Default::default()
    };
    if !docker_available(&cfg) {
        return None;
    }
    let out = run_docker(&cfg, &["system", "df", "--format", "{{json .}}"]).ok()?;
    let (build, images) = parse_system_df_reclaimable(&out);
    Some(DockerUsage {
        total_bytes: parse_system_df_total(&out),
        reclaimable_bytes: build.saturating_add(images),
    })
}

/// Sum the `Size` field across every `docker system df` resource-type row. Pure.
fn parse_system_df_total(stdout: &str) -> u64 {
    let mut total = 0u64;
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let size = v.get("Size").and_then(|s| s.as_str()).unwrap_or("");
        total = total.saturating_add(parse_human_size(size).unwrap_or(0));
    }
    total
}

/// Parse `docker system df --format '{{json .}}'` (newline-delimited JSON, one
/// object per resource type) into `(build_cache_reclaimable, images_reclaimable)`
/// bytes. The `Reclaimable` field looks like `"8.172GB (12%)"`. Pure —
/// unit-tested.
fn parse_system_df_reclaimable(stdout: &str) -> (u64, u64) {
    let mut build = 0u64;
    let mut images = 0u64;
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let typ = v.get("Type").and_then(|t| t.as_str()).unwrap_or("");
        let recl = v.get("Reclaimable").and_then(|r| r.as_str()).unwrap_or("");
        let size_str = recl.split(" (").next().unwrap_or(recl);
        let bytes = parse_human_size(size_str).unwrap_or(0);
        match typ {
            "Build Cache" => build = bytes,
            "Images" => images = bytes,
            _ => {}
        }
    }
    (build, images)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_human_size_decimal_and_binary() {
        assert_eq!(parse_human_size("0B"), Some(0));
        assert_eq!(parse_human_size("512kB"), Some(512_000));
        assert_eq!(parse_human_size("100MB"), Some(100_000_000));
        assert_eq!(parse_human_size("1.5GB"), Some(1_500_000_000));
        assert_eq!(parse_human_size("2TB"), Some(2_000_000_000_000));
        assert_eq!(parse_human_size("2GiB"), Some(2_147_483_648));
        assert_eq!(parse_human_size("  8.172GB  "), Some(8_172_000_000));
        // Bare number = bytes.
        assert_eq!(parse_human_size("42"), Some(42));
        // Garbage.
        assert_eq!(parse_human_size(""), None);
        assert_eq!(parse_human_size("GB"), None);
        assert_eq!(parse_human_size("3ZB"), None);
    }

    #[test]
    fn parse_reclaimed_finds_total_line() {
        let out = "Deleted build cache objects:\nabc123\ndef456\n\nTotal reclaimed space: 1.5GB\n";
        assert_eq!(parse_reclaimed(out), 1_500_000_000);
        assert_eq!(parse_reclaimed("nothing here\n"), 0);
        assert_eq!(parse_reclaimed("Total reclaimed space: 0B"), 0);
    }

    #[test]
    fn parse_system_df_extracts_build_and_images() {
        let out = concat!(
            r#"{"Type":"Images","TotalCount":"27","Active":"9","Size":"67.4GB","Reclaimable":"8.172GB (12%)"}"#,
            "\n",
            r#"{"Type":"Containers","TotalCount":"44","Active":"0","Size":"919.5MB","Reclaimable":"919.5MB (100%)"}"#,
            "\n",
            r#"{"Type":"Local Volumes","TotalCount":"16","Active":"13","Size":"1.959GB","Reclaimable":"0B (0%)"}"#,
            "\n",
            r#"{"Type":"Build Cache","TotalCount":"320","Active":"0","Size":"34.82GB","Reclaimable":"25.03GB"}"#,
            "\n",
        );
        let (build, images) = parse_system_df_reclaimable(out);
        assert_eq!(build, 25_030_000_000);
        assert_eq!(images, 8_172_000_000);
        // total = images + containers + volumes + build cache sizes.
        assert_eq!(parse_system_df_total(out), 105_098_500_000);
    }

    #[test]
    fn unavailable_docker_is_a_quiet_no_op() {
        let cfg = DockerCleanupConfig {
            docker_bin: "pgmcp-no-such-docker-binary-xyzzy".to_string(),
            dry_run: true,
            ..Default::default()
        };
        let report = run_docker_cleanup(&cfg);
        assert!(!report.available, "missing binary → unavailable");
        assert_eq!(report.errors, 0, "absence is benign, not an error");
        assert_eq!(report.total_bytes(), 0);
    }
}
