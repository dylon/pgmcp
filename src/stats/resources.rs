//! System + process resource telemetry for the webui Resources pane.
//!
//! Per-core CPU utilization, system memory, and load average come from `/proc`
//! directly — the daemon's target platform is Linux and `crate::stats::rss`
//! already reads `/proc`, so these need no extra dependency. GPU telemetry
//! comes from NVML (`nvml-wrapper`), which dlopens `libnvidia-ml` at runtime;
//! when no NVIDIA driver is present the GPU list is simply empty (a by-design
//! absence, logged once at `warn!` per ADR-021).
//!
//! A background sampler thread (mirroring `rss::spawn_peak_sampler`) refreshes a
//! `ResourceSnapshot` into `StatsTracker.resources` every `[webui]
//! resource_sample_secs`; `/api/resources` and the `stats?kind=resources` slice
//! read that snapshot in O(1) so request latency never pays for `/proc`/NVML.

use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::stats::rss;
use crate::stats::tracker::StatsTracker;

#[derive(Debug, Clone, Serialize, Default)]
pub struct CpuInfo {
    pub per_core_pct: Vec<f32>,
    pub aggregate_pct: f32,
    pub core_count: usize,
    pub load1: f32,
    pub load5: f32,
    pub load15: f32,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct MemoryInfo {
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub used_bytes: u64,
    pub used_pct: f32,
    pub swap_total_bytes: u64,
    pub swap_used_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct GpuInfo {
    pub index: u32,
    pub name: String,
    pub util_pct: u32,
    pub mem_total_bytes: u64,
    pub mem_used_bytes: u64,
    pub mem_used_pct: f32,
    pub temperature_c: u32,
    pub power_watts: f32,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ProcessInfo {
    pub rss_bytes: u64,
    pub peak_rss_bytes: u64,
    pub threads: u64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ResourceSnapshot {
    pub cpu: CpuInfo,
    pub memory: MemoryInfo,
    pub gpu: Vec<GpuInfo>,
    pub process: ProcessInfo,
    pub sampled_at_ms: u64,
}

#[derive(Clone, Copy)]
struct CpuTimes {
    total: u64,
    idle: u64,
}

fn parse_cpu_line(line: &str) -> Option<CpuTimes> {
    let mut it = line.split_whitespace();
    let _tag = it.next()?;
    let vals: Vec<u64> = it.filter_map(|f| f.parse().ok()).collect();
    if vals.len() < 4 {
        return None;
    }
    // idle + iowait are counted as idle time.
    let idle = vals.get(3).copied().unwrap_or(0) + vals.get(4).copied().unwrap_or(0);
    let total: u64 = vals.iter().sum();
    Some(CpuTimes { total, idle })
}

/// Per-core CPU jiffy counters from `/proc/stat` (the `cpuN` lines only; the
/// aggregate `cpu ` line is skipped so `per_core_pct` is one entry per core).
fn read_per_core_times() -> Vec<CpuTimes> {
    let Ok(data) = fs::read_to_string("/proc/stat") else {
        return Vec::new();
    };
    data.lines()
        .filter(|l| {
            l.starts_with("cpu")
                && l.as_bytes()
                    .get(3)
                    .map(|b| b.is_ascii_digit())
                    .unwrap_or(false)
        })
        .filter_map(parse_cpu_line)
        .collect()
}

fn cpu_percentages(prev: &[CpuTimes], cur: &[CpuTimes]) -> Vec<f32> {
    prev.iter()
        .zip(cur.iter())
        .map(|(p, c)| {
            let dt = c.total.saturating_sub(p.total);
            let di = c.idle.saturating_sub(p.idle);
            if dt == 0 {
                0.0
            } else {
                ((dt.saturating_sub(di) as f64 / dt as f64) * 100.0) as f32
            }
        })
        .collect()
}

fn read_loadavg() -> (f32, f32, f32) {
    fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| {
            let mut it = s.split_whitespace();
            let a = it.next()?.parse().ok()?;
            let b = it.next()?.parse().ok()?;
            let c = it.next()?.parse().ok()?;
            Some((a, b, c))
        })
        .unwrap_or((0.0, 0.0, 0.0))
}

fn read_meminfo() -> MemoryInfo {
    let mut m = MemoryInfo::default();
    let Ok(data) = fs::read_to_string("/proc/meminfo") else {
        return m;
    };
    let (mut total, mut avail, mut swap_total, mut swap_free) = (0u64, 0u64, 0u64, 0u64);
    let kb = |rest: &str| -> u64 {
        rest.split_whitespace()
            .next()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
            * 1024
    };
    for line in data.lines() {
        if let Some(r) = line.strip_prefix("MemTotal:") {
            total = kb(r);
        } else if let Some(r) = line.strip_prefix("MemAvailable:") {
            avail = kb(r);
        } else if let Some(r) = line.strip_prefix("SwapTotal:") {
            swap_total = kb(r);
        } else if let Some(r) = line.strip_prefix("SwapFree:") {
            swap_free = kb(r);
        }
    }
    m.total_bytes = total;
    m.available_bytes = avail;
    m.used_bytes = total.saturating_sub(avail);
    m.used_pct = if total > 0 {
        (m.used_bytes as f64 / total as f64 * 100.0) as f32
    } else {
        0.0
    };
    m.swap_total_bytes = swap_total;
    m.swap_used_bytes = swap_total.saturating_sub(swap_free);
    m
}

fn read_gpu(nvml: &nvml_wrapper::Nvml) -> Vec<GpuInfo> {
    use nvml_wrapper::enum_wrappers::device::TemperatureSensor;
    let count = nvml.device_count().unwrap_or(0);
    let mut out = Vec::with_capacity(count as usize);
    for index in 0..count {
        let Ok(dev) = nvml.device_by_index(index) else {
            continue;
        };
        let (mem_total, mem_used) = dev
            .memory_info()
            .map(|m| (m.total, m.used))
            .unwrap_or((0, 0));
        out.push(GpuInfo {
            index,
            name: dev.name().unwrap_or_default(),
            util_pct: dev.utilization_rates().map(|u| u.gpu).unwrap_or(0),
            mem_total_bytes: mem_total,
            mem_used_bytes: mem_used,
            mem_used_pct: if mem_total > 0 {
                (mem_used as f64 / mem_total as f64 * 100.0) as f32
            } else {
                0.0
            },
            temperature_c: dev.temperature(TemperatureSensor::Gpu).unwrap_or(0),
            power_watts: dev.power_usage().unwrap_or(0) as f32 / 1000.0,
        });
    }
    out
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// How often the sampler emits a `topic=status` realtime snapshot, independent
/// of the (finer-grained) resource sample cadence — keeps the Status pane fresh
/// without one realtime row per sample.
const REALTIME_STATUS_INTERVAL_MS: u64 = 30_000;

/// Spawn the background resource sampler. Samples every `interval_ms`,
/// computing per-core CPU % from the jiffy delta between ticks, and stores a
/// fresh `ResourceSnapshot` into `stats`. Exits when `shutdown` is set. When
/// `emitter` is `Some`, additionally emits a throttled `topic=status` realtime
/// snapshot (~every [`REALTIME_STATUS_INTERVAL_MS`]) fire-and-forget onto the
/// runtime — this thread is a plain std::thread and cannot `.await` directly.
pub fn spawn_resource_sampler(
    stats: Arc<StatsTracker>,
    shutdown: Arc<AtomicBool>,
    interval_ms: u64,
    emitter: Option<crate::realtime::RealtimeEmitter>,
) -> JoinHandle<()> {
    let interval_ms = interval_ms.max(250);
    let emit_every = (REALTIME_STATUS_INTERVAL_MS / interval_ms).max(1);
    thread::Builder::new()
        .name("pgmcp-resources".into())
        .spawn(move || {
            let mut tick: u64 = 0;
            let nvml = match nvml_wrapper::Nvml::init() {
                Ok(n) => Some(n),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "NVML unavailable; GPU telemetry disabled in the Resources view"
                    );
                    None
                }
            };
            let mut prev = read_per_core_times();
            while !shutdown.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(interval_ms));
                let cur = read_per_core_times();
                let per_core = cpu_percentages(&prev, &cur);
                prev = cur;
                let aggregate = if per_core.is_empty() {
                    0.0
                } else {
                    per_core.iter().sum::<f32>() / per_core.len() as f32
                };
                let (load1, load5, load15) = read_loadavg();
                let cpu = CpuInfo {
                    core_count: per_core.len(),
                    aggregate_pct: aggregate,
                    per_core_pct: per_core,
                    load1,
                    load5,
                    load15,
                };
                let process = ProcessInfo {
                    rss_bytes: rss::current_rss_bytes().unwrap_or(0),
                    peak_rss_bytes: stats.peak_rss_bytes.load(Ordering::Relaxed),
                    threads: rss::current_thread_count().unwrap_or(0),
                };
                let memory = read_meminfo();

                // Realtime status snapshot (topic=status), throttled to ~30s and
                // spawned fire-and-forget onto the runtime so the sampler never
                // blocks on a DB write.
                tick = tick.wrapping_add(1);
                if let Some(em) = &emitter
                    && tick.is_multiple_of(emit_every)
                {
                    em.spawn(crate::realtime::RealtimeEvent::status_snapshot(
                        process.rss_bytes,
                        aggregate,
                        memory.used_bytes,
                    ));
                }

                stats.set_resources(ResourceSnapshot {
                    cpu,
                    memory,
                    gpu: nvml.as_ref().map(read_gpu).unwrap_or_default(),
                    process,
                    sampled_at_ms: now_ms(),
                });
            }
        })
        .expect("spawn resource sampler thread")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_percentage_from_jiffy_delta() {
        // 100 total jiffies elapsed, 25 of them idle → 75% busy.
        let prev = [CpuTimes {
            total: 1000,
            idle: 800,
        }];
        let cur = [CpuTimes {
            total: 1100,
            idle: 825,
        }];
        let pct = cpu_percentages(&prev, &cur);
        assert_eq!(pct.len(), 1);
        assert!((pct[0] - 75.0).abs() < 0.01, "got {}", pct[0]);
    }

    #[test]
    fn cpu_percentage_zero_delta_is_zero() {
        let prev = [CpuTimes {
            total: 1000,
            idle: 800,
        }];
        let cur = [CpuTimes {
            total: 1000,
            idle: 800,
        }];
        assert_eq!(cpu_percentages(&prev, &cur), vec![0.0]);
    }

    #[test]
    fn meminfo_and_loadavg_are_readable_on_linux() {
        #[cfg(target_os = "linux")]
        {
            let m = read_meminfo();
            assert!(m.total_bytes > 0, "MemTotal must be positive");
            assert!(m.used_bytes <= m.total_bytes);
            let (l1, _, _) = read_loadavg();
            assert!(l1 >= 0.0);
            assert!(
                !read_per_core_times().is_empty(),
                "expected at least one core"
            );
        }
    }
}
