//! Command-spec model for the agent-driven experiment runner
//! (`pgmcp experiment run`). The agent supplies arms + a run plan (as JSON);
//! the runner executes them in the agent's own process with CPU pinning and a
//! governor check, then submits the raw samples through the protocol-enforcing
//! record path. The daemon never executes these.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One arm to measure (control / treatment / baseline).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Arm {
    pub name: String,
    /// `control | treatment | baseline` (default treatment).
    #[serde(default = "default_arm_kind")]
    pub kind: String,
    /// Argv (argv[0] is the program). Run directly (no shell) unless `shell`.
    pub command: Vec<String>,
    /// Run the command through `sh -c` (joins `command` with spaces).
    #[serde(default)]
    pub shell: bool,
    /// Working directory (must resolve under an allowed root; see runner).
    #[serde(default)]
    pub cwd: Option<String>,
    /// Extra environment variables.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Git ref this arm corresponds to (recorded, not checked out by the runner).
    #[serde(default)]
    pub git_ref: Option<String>,
}

fn default_arm_kind() -> String {
    "treatment".to_string()
}

/// How to turn one arm invocation into a metric value.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum MetricExtractor {
    /// Monotonic wall-clock of the subprocess, in milliseconds (default).
    #[default]
    WallClockMs,
    /// Maximum resident set size in KiB, via wrapping with `/usr/bin/time -v`.
    MaxRssKib,
    /// First capture group of a regex over the chosen stream → f64.
    StdoutRegex {
        pattern: String,
        #[serde(default)]
        stderr: bool,
    },
    /// RFC-6901 JSON pointer into the stdout JSON → f64.
    StdoutJsonPointer { pointer: String },
}

/// CPU-pinning + governor policy for a run (per the benchmarking mandate).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PinningSpec {
    /// Pin each arm to a fixed CPU set via `taskset -c` (default true on Linux).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Explicit cores; when absent and `enabled`, a contiguous single-CCD-sized
    /// set is chosen (see `pinning::resolve_cpus`).
    #[serde(default)]
    pub cpus: Option<Vec<usize>>,
    /// Refuse to run unless every pinned core is on the `performance` governor.
    #[serde(default = "default_true")]
    pub require_performance_governor: bool,
}

fn default_true() -> bool {
    true
}

impl Default for PinningSpec {
    fn default() -> Self {
        Self {
            enabled: true,
            cpus: None,
            require_performance_governor: true,
        }
    }
}

/// The full run plan for one experiment measurement campaign.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunPlan {
    /// Measured replicates per arm (after warm-up).
    pub replicates: usize,
    /// Warm-up replicates to discard before measuring.
    #[serde(default)]
    pub warmup: usize,
    /// Per-replicate timeout (kills the child + group on expiry).
    #[serde(default = "default_timeout_ms")]
    pub per_replicate_timeout_ms: u64,
    /// The metric name samples are recorded under.
    pub metric_name: String,
    /// How to extract the metric from each invocation.
    #[serde(default)]
    pub extractor: MetricExtractor,
    /// CPU pinning / governor policy.
    #[serde(default)]
    pub pinning: PinningSpec,
}

fn default_timeout_ms() -> u64 {
    600_000 // 10 minutes per replicate
}

/// A full run request (what `pgmcp experiment run --spec FILE` deserializes).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRequest {
    /// Target experiment (id or slug must resolve).
    #[serde(default)]
    pub experiment_id: Option<i64>,
    #[serde(default)]
    pub slug: Option<String>,
    /// Hypothesis the samples attach to.
    #[serde(default)]
    pub hypothesis_id: Option<i64>,
    pub arms: Vec<Arm>,
    pub plan: RunPlan,
    /// Run `experiment_decide` after recording (control vs treatment).
    #[serde(default)]
    pub decide: bool,
}
