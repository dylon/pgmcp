//! The agent-driven experiment runner. Executes control/treatment arms ×
//! replicates **in the caller's own process** (invoked by `pgmcp experiment
//! run`), with CPU pinning, a `performance`-governor check, warm-up discard,
//! and a per-replicate timeout (process-group kill). Returns the raw per-arm
//! sample vectors for submission through the protocol-enforcing record path.
//!
//! The daemon NEVER calls this — execution is local and agent-initiated. For
//! precise sub-millisecond timing or steady-state handling, the agent should
//! run `hyperfine`/`cargo bench` and import via `experiment_log_artifact` /
//! `pgmcp experiment ingest` (see [`crate::experiment::extract`]).

use std::collections::BTreeMap;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::extract;
use super::pinning;
use super::spec::{Arm, MetricExtractor, RunPlan};

/// The result of a run campaign.
#[derive(Debug, Clone)]
pub struct RunnerOutcome {
    /// Arm name → measured (non-warm-up) sample vector for `metric_name`.
    pub samples: BTreeMap<String, Vec<f64>>,
    pub metric_name: String,
    /// Reproducibility metadata (hardware, governor, pinned cores, …).
    pub host_meta: serde_json::Value,
    pub warnings: Vec<String>,
}

/// Whether `taskset` is callable.
fn taskset_available() -> bool {
    Command::new("taskset")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Build the argv for one invocation: `taskset -c LIST [/usr/bin/time -v] <cmd>`.
fn build_argv(arm: &Arm, extractor: &MetricExtractor, taskset_prefix: &[String]) -> Vec<String> {
    let mut argv: Vec<String> = Vec::new();
    argv.extend(taskset_prefix.iter().cloned());
    if matches!(extractor, MetricExtractor::MaxRssKib) {
        argv.push("/usr/bin/time".to_string());
        argv.push("-v".to_string());
    }
    if arm.shell {
        argv.push("sh".to_string());
        argv.push("-c".to_string());
        argv.push(arm.command.join(" "));
    } else {
        argv.extend(arm.command.iter().cloned());
    }
    argv
}

/// Run one invocation and extract its metric value.
fn run_once(arm: &Arm, plan: &RunPlan, taskset_prefix: &[String]) -> Result<f64, String> {
    let argv = build_argv(arm, &plan.extractor, taskset_prefix);
    if argv.is_empty() {
        return Err("empty command".to_string());
    }
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Own process group so the watchdog can kill the whole tree on timeout.
    cmd.process_group(0);
    if let Some(cwd) = &arm.cwd {
        cmd.current_dir(cwd);
    }
    for (k, v) in &arm.env {
        cmd.env(k, v);
    }

    let start = Instant::now();
    let child = cmd
        .spawn()
        .map_err(|e| format!("spawn `{}` failed: {e}", argv[0]))?;
    let pid = child.id();

    // Watchdog: SIGKILL the process group if the replicate exceeds the timeout.
    let done = Arc::new(AtomicBool::new(false));
    let wd_done = Arc::clone(&done);
    let timeout = plan.per_replicate_timeout_ms;
    let watchdog = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_millis(timeout);
        while Instant::now() < deadline {
            if wd_done.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if !wd_done.load(Ordering::Relaxed) {
            // Negative pid → the whole process group (pgid == pid).
            let _ = Command::new("kill")
                .arg("-9")
                .arg(format!("-{pid}"))
                .status();
        }
    });

    let output = child
        .wait_with_output()
        .map_err(|e| format!("wait failed: {e}"))?;
    let elapsed = start.elapsed();
    done.store(true, Ordering::Relaxed);
    let _ = watchdog.join();

    if !output.status.success() && !matches!(plan.extractor, MetricExtractor::MaxRssKib) {
        // /usr/bin/time -v preserves the child's exit status; for other
        // extractors a non-zero exit means the benchmark itself failed.
        return Err(format!(
            "arm '{}' exited with {:?}",
            arm.name, output.status
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    match &plan.extractor {
        MetricExtractor::WallClockMs => Ok(elapsed.as_secs_f64() * 1000.0),
        MetricExtractor::MaxRssKib => extract::parse_time_v_max_rss(&stderr),
        MetricExtractor::StdoutRegex {
            pattern,
            stderr: use_stderr,
        } => {
            let text = if *use_stderr { &stderr } else { &stdout };
            extract::extract_regex(text, pattern)
        }
        MetricExtractor::StdoutJsonPointer { pointer } => {
            extract::extract_json_pointer(&stdout, pointer)
        }
    }
}

/// Execute all arms × replicates. Enforces the governor policy up front and
/// records reproducibility metadata. Fails fast if a measured replicate errors.
pub fn execute(arms: &[Arm], plan: &RunPlan) -> Result<RunnerOutcome, String> {
    if plan.replicates == 0 {
        return Err("plan.replicates must be >= 1".to_string());
    }
    // Governor enforcement (the benchmarking mandate).
    pinning::enforce_governor(&plan.pinning)?;
    let host_meta = pinning::host_meta(&plan.pinning);

    let mut warnings = Vec::new();
    let taskset_prefix = {
        let prefix = pinning::taskset_prefix(&plan.pinning);
        if !prefix.is_empty() && !taskset_available() {
            warnings.push(
                "taskset not found; running WITHOUT CPU pinning (variance may be higher)"
                    .to_string(),
            );
            Vec::new()
        } else {
            prefix
        }
    };

    let mut samples: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for arm in arms {
        // Warm-up (discarded).
        for _ in 0..plan.warmup {
            let _ = run_once(arm, plan, &taskset_prefix);
        }
        let mut arm_samples = Vec::with_capacity(plan.replicates);
        for i in 0..plan.replicates {
            let v = run_once(arm, plan, &taskset_prefix)
                .map_err(|e| format!("arm '{}' replicate {}: {e}", arm.name, i + 1))?;
            arm_samples.push(v);
        }
        samples.insert(arm.name.clone(), arm_samples);
    }

    Ok(RunnerOutcome {
        samples,
        metric_name: plan.metric_name.clone(),
        host_meta,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::experiment::spec::PinningSpec;

    fn no_pin_plan(metric: &str, reps: usize, ex: MetricExtractor) -> RunPlan {
        RunPlan {
            replicates: reps,
            warmup: 0,
            per_replicate_timeout_ms: 5000,
            metric_name: metric.to_string(),
            extractor: ex,
            pinning: PinningSpec {
                enabled: false,
                cpus: None,
                require_performance_governor: false,
            },
        }
    }

    #[test]
    fn wall_clock_runs_true() {
        let arm = Arm {
            name: "control".to_string(),
            kind: "control".to_string(),
            command: vec!["true".to_string()],
            shell: false,
            cwd: None,
            env: Default::default(),
            git_ref: None,
        };
        let plan = no_pin_plan("wall_ms", 3, MetricExtractor::WallClockMs);
        let out = execute(std::slice::from_ref(&arm), &plan).expect("run");
        assert_eq!(out.samples["control"].len(), 3);
        assert!(out.samples["control"].iter().all(|&v| v >= 0.0));
    }

    #[test]
    fn stdout_regex_extraction() {
        let arm = Arm {
            name: "treatment".to_string(),
            kind: "treatment".to_string(),
            command: vec!["printf".to_string(), "qps=42.5\\n".to_string()],
            shell: false,
            cwd: None,
            env: Default::default(),
            git_ref: None,
        };
        let plan = no_pin_plan(
            "qps",
            2,
            MetricExtractor::StdoutRegex {
                pattern: r"qps=([0-9.]+)".to_string(),
                stderr: false,
            },
        );
        let out = execute(std::slice::from_ref(&arm), &plan).expect("run");
        assert_eq!(out.samples["treatment"], vec![42.5, 42.5]);
    }

    #[test]
    fn nonzero_exit_is_error() {
        let arm = Arm {
            name: "x".to_string(),
            kind: "treatment".to_string(),
            command: vec!["false".to_string()],
            shell: false,
            cwd: None,
            env: Default::default(),
            git_ref: None,
        };
        let plan = no_pin_plan("wall_ms", 1, MetricExtractor::WallClockMs);
        assert!(execute(std::slice::from_ref(&arm), &plan).is_err());
    }
}
