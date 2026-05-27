//! CPU pinning, frequency-governor enforcement, and host-metadata capture for
//! the experiment runner — the reproducibility requirements from the global
//! benchmarking mandates (pin to remove cross-CCD/NUMA variance; require the
//! `performance` governor; record the full hardware string).

use serde::Serialize;
use serde_json::json;

use super::spec::PinningSpec;

/// Per-core governor snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct GovernorReport {
    /// Distinct governors observed across the online CPUs.
    pub governors: Vec<String>,
    /// True iff every readable core is on `performance`.
    pub all_performance: bool,
    /// `scaling_driver` of cpu0 (e.g. `amd-pstate-epp`).
    pub driver: Option<String>,
}

fn read_first_line(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.lines().next().map(|l| l.trim().to_string()))
        .filter(|s| !s.is_empty())
}

/// Read per-core `scaling_governor` across the online CPUs.
pub fn read_governors() -> GovernorReport {
    let n = num_cpus();
    let mut seen: Vec<String> = Vec::new();
    let mut all_performance = n > 0;
    let mut any = false;
    for cpu in 0..n {
        let path = format!("/sys/devices/system/cpu/cpu{cpu}/cpufreq/scaling_governor");
        if let Some(g) = read_first_line(&path) {
            any = true;
            if g != "performance" {
                all_performance = false;
            }
            if !seen.contains(&g) {
                seen.push(g);
            }
        }
    }
    if !any {
        all_performance = false; // could not read any governor
    }
    let driver = read_first_line("/sys/devices/system/cpu/cpu0/cpufreq/scaling_driver");
    GovernorReport {
        governors: seen,
        all_performance,
        driver,
    }
}

/// Number of online CPUs (best-effort).
pub fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Resolve the CPU set to pin to. Explicit `cpus` win; otherwise a contiguous
/// single-CCD-sized set (first `min(8, ncpu)` cores) — a portable default that
/// keeps an arm on one L3 domain. `None` ⇒ no pinning.
pub fn resolve_cpus(spec: &PinningSpec) -> Option<Vec<usize>> {
    if !spec.enabled {
        return None;
    }
    if let Some(cpus) = &spec.cpus {
        if cpus.is_empty() {
            return None;
        }
        return Some(cpus.clone());
    }
    let n = num_cpus().clamp(1, 8);
    Some((0..n).collect())
}

/// The `taskset -c <list>` argv prefix for the resolved CPU set (empty when no
/// pinning, or when `taskset` is unavailable — the caller checks availability).
pub fn taskset_prefix(spec: &PinningSpec) -> Vec<String> {
    match resolve_cpus(spec) {
        Some(cpus) if !cpus.is_empty() => {
            let list = cpus
                .iter()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join(",");
            vec!["taskset".to_string(), "-c".to_string(), list]
        }
        _ => Vec::new(),
    }
}

/// Enforce the governor policy. `Err(msg)` when `require_performance_governor`
/// is set and not every pinned/online core is on `performance`.
pub fn enforce_governor(spec: &PinningSpec) -> Result<GovernorReport, String> {
    let report = read_governors();
    if spec.require_performance_governor && !report.all_performance {
        return Err(format!(
            "CPU governor policy violated: observed {:?}, require all `performance`. \
             Run `sudo cpupower frequency-set -g performance` (or set require_performance_governor=false).",
            report.governors
        ));
    }
    Ok(report)
}

fn cpu_model() -> Option<String> {
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    for line in cpuinfo.lines() {
        if let Some((k, v)) = line.split_once(':')
            && k.trim() == "model name"
        {
            return Some(v.trim().to_string());
        }
    }
    None
}

fn uname() -> Option<String> {
    std::process::Command::new("uname")
        .arg("-a")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// First H1 of `~/.claude/hardware-specifications.md`, mirroring
/// `scripts/measure_recovery_times.sh`'s hardware stamping.
fn hardware_headline() -> Option<String> {
    let home = dirs::home_dir()?;
    let path = home.join(".claude/hardware-specifications.md");
    let content = std::fs::read_to_string(path).ok()?;
    content
        .lines()
        .find(|l| l.trim_start().starts_with("# "))
        .map(|l| l.trim_start().trim_start_matches("# ").trim().to_string())
}

/// Capture the reproducibility metadata recorded on each run.
pub fn host_meta(spec: &PinningSpec) -> serde_json::Value {
    let gov = read_governors();
    json!({
        "cpus_pinned": resolve_cpus(spec),
        "governors": gov.governors,
        "all_performance": gov.all_performance,
        "scaling_driver": gov.driver,
        "num_cpus": num_cpus(),
        "cpu_model": cpu_model(),
        "uname": uname(),
        "hardware": hardware_headline(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_cpus_defaults_to_contiguous_set() {
        let spec = PinningSpec::default();
        let cpus = resolve_cpus(&spec).expect("default pins");
        assert!(!cpus.is_empty());
        assert_eq!(cpus[0], 0);
    }

    #[test]
    fn disabled_pinning_yields_no_prefix() {
        let spec = PinningSpec {
            enabled: false,
            cpus: None,
            require_performance_governor: false,
        };
        assert!(taskset_prefix(&spec).is_empty());
        assert!(resolve_cpus(&spec).is_none());
    }

    #[test]
    fn explicit_cpus_taskset_prefix() {
        let spec = PinningSpec {
            enabled: true,
            cpus: Some(vec![2, 3, 4]),
            require_performance_governor: false,
        };
        assert_eq!(
            taskset_prefix(&spec),
            vec!["taskset".to_string(), "-c".to_string(), "2,3,4".to_string()]
        );
    }
}
