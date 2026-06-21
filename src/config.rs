use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

mod metrics;
pub use metrics::*;

use toml::Value as TomlValue;

use crate::error::{PgmcpError, Result};

/// Top-level configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub workspace: WorkspaceConfig,
    #[serde(default)]
    pub indexer: IndexerConfig,
    #[serde(default)]
    pub database: DatabaseConfig,
    #[serde(default)]
    pub embeddings: EmbeddingsConfig,
    #[serde(default)]
    pub vector: VectorConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub metrics: MetricsConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub work_pool: WorkPoolConfig,
    #[serde(default)]
    pub cron: CronConfig,
    #[serde(default)]
    pub system: SystemConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    /// `[disk_guard]` — pressure-driven disk-space watchdog (src/health). Pauses
    /// pgmcp's own disk-growing work and triggers `target-cleanup` out-of-band
    /// when a watched filesystem runs low on bytes or inodes. On by default
    /// (`pause_floor_gb = 0` disables).
    #[serde(default)]
    pub disk_guard: DiskGuardConfig,
    /// `[outbox]` — durable store-and-forward for fire-and-forget hook ingress
    /// while the DB is down (src/health). Replayed on recovery. On by default,
    /// self-limiting (capped + own-filesystem self-floor).
    #[serde(default)]
    pub outbox: OutboxConfig,
    /// `[fuzzy]` — disk-backed PersistentARTrieChar fuzzy-index layout.
    /// Populated in Phase 4 of the integration plan
    /// `~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md`.
    #[serde(default)]
    pub fuzzy: FuzzyConfig,
    /// `[api]` — REST API / RAG-hook tuning (optional cross-encoder rerank).
    #[serde(default)]
    pub api: ApiConfig,
    /// `[a2a]` — inter-agent best-practice exchange + RLM knobs. All
    /// fields default off/inert so a stock pgmcp install behaves as before.
    #[serde(default)]
    pub a2a: A2aConfig,
    /// `[nudges]` — JIT adoption-nudge tuning (observe pipeline). Off by default.
    #[serde(default)]
    pub nudges: NudgesConfig,
    /// `[digest]` — proactive-surfacing digest (Phase 4). Rides the SessionStart
    /// `pgmcp context` CLI and the UserPromptSubmit observe `additional_context`
    /// to push tracker/health/trend state. Read-only (SELECTs + its own
    /// rate-limit ledger insert). Off by default (local-first).
    #[serde(default)]
    pub digest: DigestConfig,
    /// `[experiments]` — scientific-experiment subsystem defaults (acceptance
    /// α, statistical test, power target, ledger rendering, CPU-governor
    /// enforcement). All read per-call via the live `ArcSwap<Config>`.
    #[serde(default)]
    pub experiments: ExperimentsConfig,
    /// `[tracker]` — work-item / plan tracker config. `user_token` gates the
    /// user-authority operations (`work_item_defer`/`work_item_reinstate`) so
    /// an agent cannot self-defer (scope-cut): the user supplies the token; the
    /// MCP agent path does not have it.
    #[serde(default)]
    pub tracker: TrackerConfig,
    /// `[ontology]` — the hierarchical-ontology subsystem (invariant mining, FCA
    /// hierarchy build, catalog migration, analyzer-finding integration,
    /// constraint reasoning, Poincaré link-prediction). Runs by default.
    #[serde(default)]
    pub ontology: OntologyConfig,
    /// `[clients]` — MCP-client tracking (PID/cwd/project/liveness) + file-event
    /// attribution. Capture is on by default (local-first telemetry); the eBPF
    /// and `/proc`-fd file-event sources are opt-in.
    #[serde(default)]
    pub clients: ClientsConfig,
    /// `[worklog]` — defaults for the `work_summary` tool (period work summaries
    /// over a workspace's git repos). Per-call params always override these.
    #[serde(default)]
    pub worklog: WorklogConfig,
    /// `[security_scan]` — external security-scanner subsystem
    /// (`src/cron/security_scan.rs`): runs installed scanners (gitleaks, semgrep,
    /// trivy, cargo-audit, …) over each indexed project, persisting findings to
    /// `external_scanner_findings`. The scheduled cron is off by default; the
    /// on-demand `security_scan` MCP tool works regardless.
    #[serde(default)]
    pub security_scan: SecurityScanConfig,
    /// `[tape]` — defaults for the Crucible context-tape paging control plane
    /// (`src/tape/`): the per-session token budget, eviction policy, and logical
    /// TTL used when a session does not specify its own `working_set_config`.
    /// Pure coordination/MEMORY; no shell, no user-file writes.
    #[serde(default)]
    pub tape: TapeConfig,
}

/// `[tape]` — defaults for the context-tape paging control plane
/// (`src/tape/`). A session may override any of these via its
/// `working_set_config` row; these supply the fallbacks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TapeConfig {
    /// Default token budget the resident working set must not exceed when a
    /// session does not specify one (default 128_000 — a typical large-model
    /// window). The controller evicts mechanically to keep within this.
    #[serde(default = "default_tape_budget_tokens")]
    pub budget_tokens: i32,
    /// Default eviction policy label (one of [`crate::tape::vocab::EvictionPolicy`]
    /// — `lru`/`lfu`/`ttl`/`fifo`/`cost_aware`/`importance_weighted`). Default
    /// `importance_weighted`. An unrecognized value falls back to the default at
    /// read time.
    #[serde(default = "default_tape_policy")]
    pub policy: String,
    /// Default logical TTL in clock ticks for the `ttl` policy — a page whose
    /// logical age (clock delta, never wall-time) exceeds this is eligible for
    /// TTL eviction. `0` (default) disables logical-TTL eviction.
    #[serde(default)]
    pub ttl_secs: i32,
    /// Whether a dirty page's write-back (`RealTapeDataPlane::put`) is allowed to
    /// **promote** an agent-edited corpus page into durable memory as a
    /// bi-temporal supersession in `memory_observations` (never `file_chunks` —
    /// the corpus is read-only). `false` (default) makes `put` a no-op DB-side:
    /// the mutated bytes live only in the per-tree `TapeStore` (the hot/OOC tier)
    /// and are discarded on eviction, so a stray write can never leak into durable
    /// memory. Operators opt in explicitly (`[tape] allow_promotion = true`) when
    /// they want agent scratch edits to survive as observations.
    #[serde(default)]
    pub allow_promotion: bool,
    /// Idle-tree reaper: a per-tree `TapeStore` not touched for this many seconds
    /// is reclaimed by the `tape-store-reaper` cron — the backstop for a recursion
    /// tree that ended without an explicit `drop_tree` (e.g. a crashed run). `0`
    /// (default) disables it. Wall-clock by design: it only frees RAM and never
    /// affects a resume's reconstructed residency (which is replay-deterministic
    /// from the LOGICAL clock, not wall-time).
    #[serde(default)]
    pub reaper_idle_secs: u64,
    /// How often the `tape-store-reaper` cron runs, in seconds. `0` (default)
    /// disables the cron. BOTH `reaper_interval_secs` and `reaper_idle_secs` must
    /// be `> 0` for any reaping to occur.
    #[serde(default)]
    pub reaper_interval_secs: u64,
}

impl Default for TapeConfig {
    fn default() -> Self {
        Self {
            budget_tokens: default_tape_budget_tokens(),
            policy: default_tape_policy(),
            ttl_secs: 0,
            allow_promotion: false,
            reaper_idle_secs: 0,
            reaper_interval_secs: 0,
        }
    }
}

fn default_tape_budget_tokens() -> i32 {
    128_000
}

fn default_tape_policy() -> String {
    crate::tape::vocab::EvictionPolicy::ImportanceWeighted
        .as_str()
        .to_string()
}

/// `[security_scan]` — the external security-scanner subsystem
/// (`src/cron/security_scan.rs`). Runs installed scanners (gitleaks, semgrep,
/// trivy, cargo-audit, …) over each indexed project, persisting findings to
/// `external_scanner_findings`. Off by default (local-first; opt-in).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SecurityScanConfig {
    /// Master switch for the scheduled cron (default false). The on-demand
    /// `security_scan` MCP tool works regardless of this flag.
    #[serde(default)]
    pub enabled: bool,
    /// Cron cadence in seconds (default 604800 = weekly). 0 disables the cron.
    #[serde(default = "default_security_scan_interval")]
    pub cron_interval_secs: u64,
    /// Scanner allow-list: when non-empty, only these scanners run. Empty
    /// (default) = every applicable scanner that is installed. Names match the
    /// tool-card slugs (gitleaks, trufflehog, detect-secrets, semgrep, bandit,
    /// cppcheck, clang-tidy, cargo-audit, cargo-deny, grype, trivy, hadolint, syft).
    #[serde(default)]
    pub tools: Vec<String>,
    /// Per-scanner, per-project wall-clock timeout in seconds (default 300). A
    /// scanner exceeding it is killed and recorded with `status='timeout'`.
    #[serde(default = "default_security_scan_timeout")]
    pub per_project_timeout_secs: u64,
    /// Maximum scanner subprocesses running concurrently across the sweep
    /// (default 1 — gentle on a busy workstation).
    #[serde(default = "default_security_scan_concurrency")]
    pub max_concurrent: usize,
    /// Project-name substrings to exclude from the sweep (case-insensitive).
    #[serde(default)]
    pub exclude_projects: Vec<String>,
    /// When true, skip scanners that fetch advisory/vuln DBs or rule packs over
    /// the network (cargo-audit, grype, trivy, semgrep `--config auto`) for
    /// air-gapped operation; the fully-local scanners still run (default false).
    /// Source is never uploaded either way — this only governs DB/rule fetches.
    #[serde(default)]
    pub offline_only: bool,
}

impl Default for SecurityScanConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            cron_interval_secs: default_security_scan_interval(),
            tools: Vec::new(),
            per_project_timeout_secs: default_security_scan_timeout(),
            max_concurrent: default_security_scan_concurrency(),
            exclude_projects: Vec::new(),
            offline_only: false,
        }
    }
}

fn default_security_scan_interval() -> u64 {
    604_800
}
fn default_security_scan_timeout() -> u64 {
    300
}
fn default_security_scan_concurrency() -> usize {
    1
}

/// `[worklog]` — defaults for the `work_summary` MCP tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorklogConfig {
    /// Output format when the call omits `format` (markdown|org|json).
    #[serde(default = "default_worklog_format")]
    pub default_format: String,
    /// Author filter when the call omits `author`. `None` resolves the local
    /// `git config user.name` ("my work"); set to "all" for every contributor.
    #[serde(default)]
    pub default_author: Option<String>,
    /// Temporal-graph enrichment mode when omitted (auto|on|off).
    #[serde(default = "default_worklog_graph")]
    pub graph_enrichment: String,
    /// Default for `narrative` (deterministic prose bullets) when omitted.
    #[serde(default)]
    pub narrative_default: bool,
    /// Cap on repos scanned when the call omits `max_repos`.
    #[serde(default = "default_worklog_max_repos")]
    pub max_repos: u32,
    /// Cap on projects rendered when the call omits `limit`.
    #[serde(default = "default_worklog_max_projects")]
    pub max_projects: u32,
    /// Local LLM backend for `narrative=true` prose generation: `qwen3-4b`
    /// (default, ~2.5 GB Q4_K_M) or `qwen3-8b` (~5 GB). Any other value — or an
    /// unavailable model / GPU — falls back to deterministic prose. The model is
    /// loaded (from the HuggingFace cache) only when a call sets `narrative=true`.
    #[serde(default = "default_worklog_narrative_backend")]
    pub narrative_backend: String,
    /// Token budget per project for narrative generation (default 160).
    #[serde(default = "default_worklog_narrative_max_tokens")]
    pub narrative_max_tokens: u32,
}

impl Default for WorklogConfig {
    fn default() -> Self {
        Self {
            default_format: default_worklog_format(),
            default_author: None,
            graph_enrichment: default_worklog_graph(),
            narrative_default: false,
            max_repos: default_worklog_max_repos(),
            max_projects: default_worklog_max_projects(),
            narrative_backend: default_worklog_narrative_backend(),
            narrative_max_tokens: default_worklog_narrative_max_tokens(),
        }
    }
}

fn default_worklog_format() -> String {
    "markdown".to_string()
}
fn default_worklog_graph() -> String {
    "auto".to_string()
}
fn default_worklog_max_repos() -> u32 {
    200
}
fn default_worklog_max_projects() -> u32 {
    100
}
fn default_worklog_narrative_backend() -> String {
    "qwen3-4b".to_string()
}
fn default_worklog_narrative_max_tokens() -> u32 {
    160
}

/// `[clients]` — MCP-client tracking and file-event attribution knobs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientsConfig {
    /// Master switch for client capture (`note_client` + the liveness cron's
    /// effect). When false no `mcp_clients` rows are written and the listen port
    /// is never registered, so `note_client` short-circuits.
    #[serde(default = "default_true_clients")]
    pub enabled: bool,
    /// Accept `POST /api/client/file_event` (the Claude Code PostToolUse hook)
    /// and record `client_file_events`. When false the endpoint is a no-op.
    #[serde(default = "default_true_clients")]
    pub file_events: bool,
    /// Run the eBPF PID-filtered file-event probe (Phase 2B). Requires
    /// `CAP_BPF`+`CAP_PERFMON` (or root) and `bpftrace` on PATH; off by default.
    #[serde(default)]
    pub ebpf_enabled: bool,
    /// How often (seconds) the eBPF consumer re-reads the live client PID set and
    /// respawns its probe when the set changed. Default 15.
    #[serde(default = "default_ebpf_refresh_secs")]
    pub ebpf_refresh_secs: u64,
    /// Collapse window (seconds) for identical `(pid, op, path)` eBPF events, so
    /// a tight open/reopen loop records one row, not hundreds. Default 5.
    #[serde(default = "default_ebpf_dedup_secs")]
    pub ebpf_dedup_secs: u64,
    /// Sample `/proc/<pid>/fd` on each liveness tick as a best-effort file-event
    /// supplement (the `proc_fd` source). Off by default — near-blind to
    /// open-close editors, so cheap but low-signal.
    #[serde(default)]
    pub proc_fd_supplement: bool,
}

impl Default for ClientsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            file_events: true,
            ebpf_enabled: false,
            ebpf_refresh_secs: default_ebpf_refresh_secs(),
            ebpf_dedup_secs: default_ebpf_dedup_secs(),
            proc_fd_supplement: false,
        }
    }
}

fn default_ebpf_refresh_secs() -> u64 {
    15
}

fn default_ebpf_dedup_secs() -> u64 {
    5
}

fn default_true_clients() -> bool {
    true
}

/// `[ontology]` — the hierarchical-ontology subsystem's tuning knobs. The crons
/// run by default (there is no enable switch — they follow the
/// `memory-graph-refresh` convention); set `cron_interval_secs = 0` to disable
/// every ontology cron. Deterministic and idempotent, so they are safe to leave on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OntologyConfig {
    /// Interval between ontology cron passes (invariant mining, hierarchy build,
    /// catalog migration, analyzer-finding integration, constraint reasoning,
    /// link-prediction). `0` disables every ontology cron.
    #[serde(default = "default_ontology_interval")]
    pub cron_interval_secs: u64,
    /// Max items (concepts / invariants) processed per cron run.
    #[serde(default = "default_ontology_max_per_run")]
    pub max_items_per_run: i64,
}

impl Default for OntologyConfig {
    fn default() -> Self {
        Self {
            cron_interval_secs: default_ontology_interval(),
            max_items_per_run: default_ontology_max_per_run(),
        }
    }
}

fn default_ontology_interval() -> u64 {
    86400
}

fn default_ontology_max_per_run() -> i64 {
    500
}

/// `[tracker]` configuration. Inert by default: with no `user_token`, the
/// defer/reinstate tools are disabled (they tell the user to set it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TrackerConfig {
    /// Shared secret authorizing `work_item_defer`/`work_item_reinstate`. Keep
    /// it in the user's local config; do not reveal it to the agent. `None`
    /// (default) disables those user-authority operations.
    #[serde(default)]
    pub user_token: Option<String>,
}

/// `[experiments]` — defaults the experiment-protocol engine prescribes and
/// the runner/decide path consults. Read live (effectively hot) via
/// `ctx.config().load()`. See
/// `~/.claude/plans/plan-how-to-effectively-drifting-fox.md`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentsConfig {
    /// Default significance level for pre-registered NHST criteria.
    #[serde(default = "default_experiment_alpha")]
    pub default_alpha: f64,
    /// Default two-sample test when the metric is stochastic ("welch_t" or
    /// "mann_whitney_u"); the protocol may override per normality.
    #[serde(default = "default_experiment_test")]
    pub default_test: String,
    /// Statistical power target used to size `required_samples_per_arm`.
    #[serde(default = "default_experiment_power")]
    pub default_power: f64,
    /// Hard floor on samples/arm for stochastic metrics, regardless of the
    /// power calculation (benchmark tails need replicates — Kalibera-Jones).
    #[serde(default = "default_experiment_min_samples")]
    pub min_samples_per_arm: u32,
    /// Default multiple-comparison correction for multi-metric composites
    /// ("benjamini_hochberg", "bonferroni", or "none").
    #[serde(default = "default_experiment_correction")]
    pub default_correction: String,
    /// Embed experiment/hypothesis/decision text synchronously on write so
    /// `experiment_search` works immediately (the cron also backfills NULLs).
    #[serde(default = "default_true")]
    pub embed_on_write: bool,
    /// Render the committed markdown ledger automatically on
    /// `experiment_decide`. Off by default so the daemon stays out of the
    /// working tree unless asked (`experiment_render_ledger` is explicit).
    #[serde(default)]
    pub auto_render_ledger: bool,
    /// Directory (relative to a project root) for rendered ledgers.
    #[serde(default = "default_experiment_ledger_dir")]
    pub ledger_dir: String,
    /// When the CLI runner executes arms, refuse to run unless every pinned
    /// CPU is on the `performance` governor (per the benchmarking mandate).
    #[serde(default = "default_true")]
    pub require_performance_governor: bool,
    /// Whether a **verified positive** experiment decision may be promoted into
    /// durable memory as a bi-temporal supersession in `memory_observations`
    /// (the P9 context-tape pre-registration's promotion path — see
    /// [`crate::experiment::context_tape::promote_decision`]). `false` (default)
    /// makes promotion a no-op: a verified positive decision is recorded in the
    /// `experiment_*` tables but never written to the memory graph, so an
    /// accepted result can never silently mutate durable memory. Mirrors the
    /// `[tape] allow_promotion` write-back gate and the tracker's "no agent arm
    /// into `verified`" boundary: promotion is gated on a real server-computed
    /// decision AND this explicit opt-in. Operators set
    /// `[experiments] allow_promotion = true` to let accepted pre-registered
    /// results graduate into memory.
    #[serde(default)]
    pub allow_promotion: bool,
}

fn default_experiment_alpha() -> f64 {
    0.05
}
fn default_experiment_test() -> String {
    "welch_t".to_string()
}
fn default_experiment_power() -> f64 {
    0.8
}
fn default_experiment_min_samples() -> u32 {
    30
}
fn default_experiment_correction() -> String {
    "benjamini_hochberg".to_string()
}
fn default_experiment_ledger_dir() -> String {
    "docs/scientific-ledger".to_string()
}

impl Default for ExperimentsConfig {
    fn default() -> Self {
        Self {
            default_alpha: default_experiment_alpha(),
            default_test: default_experiment_test(),
            default_power: default_experiment_power(),
            min_samples_per_arm: default_experiment_min_samples(),
            default_correction: default_experiment_correction(),
            embed_on_write: default_true(),
            auto_render_ledger: false,
            ledger_dir: default_experiment_ledger_dir(),
            require_performance_governor: default_true(),
            allow_promotion: false,
        }
    }
}

/// `[a2a]` — Agent-to-Agent best-practice exchange + RLM tuning. Every
/// field defaults off/inert: a stock pgmcp install neither writes peer
/// outcomes to the memory graph nor injects them into prompts until the
/// operator opts in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct A2aConfig {
    /// When true, the A2A pattern tools and the dispatcher distill each
    /// peer artifact / task outcome into the shared `memory_*` graph
    /// (Part A write-back seam).
    #[serde(default)]
    pub writeback_enabled: bool,
    /// When true, peer best practices are retrieved and injected into
    /// pattern role prompts and the `/api/session/observe`
    /// `additional_context` (Part A read-before-act).
    #[serde(default)]
    pub inject_best_practices: bool,
    /// When true, the linear collaboration patterns (sequential / mixture /
    /// distillation) drive their peer calls from the CFSM/MPST protocol via the
    /// `csm::driver::ProtocolDriver` (ADR-009 Phase 6) instead of the hardcoded
    /// async order — conformant by construction. Default off: the hardcoded
    /// path is unchanged.
    #[serde(default)]
    pub protocol_interpreter: bool,
    /// When true, the daemon auto-spawns in-process claude + codex A2A leaf
    /// adapters at startup (ports 3201 / 3202) and self-registers them, so the
    /// peer registry is non-empty and `a2a_pattern_*` /
    /// `a2a_find_agents_by_specialty` resolve out of the box. The leaf children
    /// run with pgmcp's MCP disabled (see the adapter commands), so they cannot
    /// re-enter the pattern tools. Default off (stock install stays inert).
    #[serde(default)]
    pub autostart_adapters: bool,
    /// Phase-4 (ADR-009 §4.6): surface a proactive "⚠ a dependency you rely on is
    /// being edited (dirty) by <agent>" warning into the dependent's
    /// `session_observe` `additional_context`, for dependencies that are dirty,
    /// have a live editor, and are not yet under an open coordination request from
    /// this project. Off by default. Read-only; budget-shared with the 2 KB block.
    #[serde(default)]
    pub proactive_dependency_warnings: bool,
    #[serde(default)]
    pub reflection: A2aReflectionConfig,
    #[serde(default)]
    pub rlm: A2aRlmConfig,
    #[serde(default)]
    pub recursion: A2aRecursionConfig,
    #[serde(default)]
    pub csm_validate: A2aCsmValidateConfig,
}

/// `[a2a.recursion]` — Tier-2 Recursive-TextMAS defaults (ADR-009). Off by
/// default (`default_rounds = 1` ⇒ single pass); per-call params override.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct A2aRecursionConfig {
    #[serde(default = "default_a2a_recursion_rounds")]
    pub default_rounds: u32,
    #[serde(default = "default_a2a_recursion_carry")]
    pub carry: String,
    #[serde(default = "default_a2a_recursion_marker")]
    pub converge_marker: String,
}

impl Default for A2aRecursionConfig {
    fn default() -> Self {
        Self {
            default_rounds: default_a2a_recursion_rounds(),
            carry: default_a2a_recursion_carry(),
            converge_marker: default_a2a_recursion_marker(),
        }
    }
}

fn default_a2a_recursion_rounds() -> u32 {
    1
}
fn default_a2a_recursion_carry() -> String {
    "final_answer_only".to_string()
}
fn default_a2a_recursion_marker() -> String {
    "CONVERGED".to_string()
}

/// `[a2a.reflection]` — cross-agent consensus reflection + promotion
/// (Part A phase A4). Mirrors `[memory.reflection]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct A2aReflectionConfig {
    /// Whether the periodic `a2a-reflect` cron runs.
    #[serde(default)]
    pub cron_enabled: bool,
    #[serde(default = "default_a2a_reflection_interval")]
    pub cron_interval_secs: u64,
    /// Minimum distinct agents that must agree on the same
    /// approach+outcome before it enters the shared (agent_id=NULL)
    /// scope. The core anti-flooding gate.
    #[serde(default = "default_a2a_min_agents")]
    pub min_agents: i64,
    /// Minimum mean confidence for the consensus gate.
    #[serde(default = "default_a2a_min_confidence")]
    pub min_confidence: f32,
    /// Promote a practice to workspace scope (cross-task reach) when it
    /// has agreeing reports across at least this many distinct projects.
    #[serde(default = "default_a2a_workspace_promotion")]
    pub workspace_promotion: i64,
    /// Trust-weighted promotion-score threshold for `durable_mandates`.
    #[serde(default = "default_a2a_promote_threshold")]
    pub promote_threshold: f32,
    /// When true, promotion also appends a bullet under the
    /// `## Agreed best practices (pgmcp)` marker in the target file
    /// (AGENTS.md/CLAUDE.md). Default false — DB-only.
    #[serde(default)]
    pub write_to_file: bool,
}

impl Default for A2aReflectionConfig {
    fn default() -> Self {
        Self {
            cron_enabled: false,
            cron_interval_secs: default_a2a_reflection_interval(),
            min_agents: default_a2a_min_agents(),
            min_confidence: default_a2a_min_confidence(),
            workspace_promotion: default_a2a_workspace_promotion(),
            promote_threshold: default_a2a_promote_threshold(),
            write_to_file: false,
        }
    }
}

/// `[a2a.csm_validate]` — the CSM auto-conformance cron (ADR-009). Scans
/// completed `a2a_pattern_*` runs with no `csm_run_traces` row yet and validates
/// them, closing the learning loop without depending on an agent calling
/// `csm_validate_run`. Off by default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct A2aCsmValidateConfig {
    /// Whether the periodic `csm-validate` cron runs.
    #[serde(default)]
    pub cron_enabled: bool,
    #[serde(default = "default_csm_validate_interval")]
    pub cron_interval_secs: u64,
    /// Max runs validated per cron tick (bounds the first-enable backlog).
    #[serde(default = "default_csm_validate_batch")]
    pub batch_limit: i64,
}

impl Default for A2aCsmValidateConfig {
    fn default() -> Self {
        Self {
            cron_enabled: false,
            cron_interval_secs: default_csm_validate_interval(),
            batch_limit: default_csm_validate_batch(),
        }
    }
}

fn default_csm_validate_interval() -> u64 {
    1800
}
fn default_csm_validate_batch() -> i64 {
    200
}

/// `[nudges]` — JIT adoption-nudge tuning for the `/api/session/observe`
/// pipeline (Claude-only — only clients running the observe hook reach it).
/// Off by default so a stock install stays inert.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NudgesConfig {
    /// Whether the prompt classifier appends a single tool-family nudge to
    /// `additional_context`.
    #[serde(default)]
    pub enabled: bool,
    /// Suppress re-nudging the same `(session, family)` within this many seconds.
    #[serde(default = "default_nudge_ttl_secs")]
    pub ttl_secs: u64,
    /// Lifetime cap on nudges of a given family per session.
    #[serde(default = "default_nudge_max_per_session")]
    pub max_per_session: u32,
}

impl Default for NudgesConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ttl_secs: default_nudge_ttl_secs(),
            max_per_session: default_nudge_max_per_session(),
        }
    }
}

fn default_nudge_ttl_secs() -> u64 {
    180
}
fn default_nudge_max_per_session() -> u32 {
    3
}

/// `[digest]` — proactive-surfacing digest tuning (Phase 4). The digest rides
/// the two channels agents already read — the SessionStart `pgmcp context` CLI
/// and the UserPromptSubmit `/api/session/observe` `additional_context` — and
/// surfaces tracker state (overdue / blocked / needs-triage / next-actionable),
/// pgmcp health (index staleness, embedding backlog, cron failures), and quality
/// trends. It is structurally read-only: only `SELECT`s plus a single insert
/// into its own `digest_emissions` rate-limit ledger. Off by default so a stock
/// install stays inert; `webhook_url` is empty (no outbound) by default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DigestConfig {
    /// Master switch. When false the digest is never composed or appended on any
    /// channel (the local-first default).
    #[serde(default)]
    pub enabled: bool,
    /// Append the digest to the SessionStart `pgmcp context` CLI output.
    #[serde(default = "default_true_digest")]
    pub session_start: bool,
    /// Append the digest to the UserPromptSubmit observe `additional_context`.
    #[serde(default = "default_true_digest")]
    pub prompt: bool,
    /// Suppress re-emitting an identical digest (same `content_sha256`) to the
    /// same session within this many seconds (dedup window).
    #[serde(default = "default_digest_ttl_secs")]
    pub ttl_secs: u64,
    /// Lifetime cap on digest emissions per session (across channels).
    #[serde(default = "default_digest_max_per_session")]
    pub max_per_session: u32,
    /// Byte budget for the rendered digest Markdown block. Severity-sorted items
    /// are dropped once the budget is hit.
    #[serde(default = "default_digest_max_bytes")]
    pub max_bytes: usize,
    /// Include the TREND section (Phase-1 GPA slope / forecast). When false the
    /// digest carries only TRACKER + HEALTH.
    #[serde(default = "default_true_digest")]
    pub include_trends: bool,
    /// Include the CONCURRENCY pillar (open deadlock cycles, trending lock
    /// contention, newest high-severity concurrency findings). Off by default —
    /// the `concurrency-scan` cron is itself opt-in, so the pillar is empty until
    /// at least two health snapshots exist.
    #[serde(default)]
    pub include_concurrency: bool,
    /// Include the ONTOLOGY pillar — design invariants governing files in scope
    /// (the read-only constraint-surfacing / anti-mistake path). On by default.
    #[serde(default = "default_true_digest")]
    pub include_ontology: bool,
    /// Optional outbound webhook. Empty (the default) = no outbound POST. When
    /// set, the daemon fires the digest (fire-and-forget) on the observe path,
    /// gated on `max_severity() >= webhook_min_severity`.
    #[serde(default)]
    pub webhook_url: String,
    /// Minimum digest `max_severity()` to POST to `webhook_url` (one of
    /// `info|notice|high|critical`).
    #[serde(default = "default_digest_webhook_min_severity")]
    pub webhook_min_severity: String,
    /// Emit a `pg_notify('pgmcp_digest', …)` on the daemon path when a digest is
    /// composed. Off by default; reserved wiring point (no SSE consumer is built
    /// in the single-user setup — see `src/digest/mod.rs`).
    #[serde(default)]
    pub pg_notify: bool,
}

impl Default for DigestConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            session_start: default_true_digest(),
            prompt: default_true_digest(),
            ttl_secs: default_digest_ttl_secs(),
            max_per_session: default_digest_max_per_session(),
            max_bytes: default_digest_max_bytes(),
            include_trends: default_true_digest(),
            include_concurrency: false,
            include_ontology: default_true_digest(),
            webhook_url: String::new(),
            webhook_min_severity: default_digest_webhook_min_severity(),
            pg_notify: false,
        }
    }
}

fn default_true_digest() -> bool {
    true
}
fn default_digest_ttl_secs() -> u64 {
    1800
}
fn default_digest_max_per_session() -> u32 {
    10
}
fn default_digest_max_bytes() -> usize {
    1024
}
fn default_digest_webhook_min_severity() -> String {
    "high".to_string()
}

fn default_a2a_reflection_interval() -> u64 {
    86400
}
fn default_a2a_min_agents() -> i64 {
    2
}
fn default_a2a_min_confidence() -> f32 {
    0.6
}
fn default_a2a_workspace_promotion() -> i64 {
    2
}
fn default_a2a_promote_threshold() -> f32 {
    0.5
}

/// `[a2a.rlm]` — Recursive-Language-Model knobs (Parts D + E). Safe
/// defaults; depth>1 is opt-in per call (default `rlm_depth = 1`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct A2aRlmConfig {
    /// Max recursion depth a run may request (clamped to the hard cap).
    #[serde(default = "default_rlm_max_depth")]
    pub max_depth: u32,
    /// Default total sub-call budget across a recursion tree.
    #[serde(default = "default_rlm_max_budget")]
    pub max_budget: u32,
    /// MSM neighbors examined per candidate strategy in the chooser.
    #[serde(default = "default_rlm_neighbor_k")]
    pub neighbor_k: usize,
    /// Epsilon for explore/exploit over decomposition strategies.
    #[serde(default = "default_rlm_explore_epsilon")]
    pub explore_epsilon: f32,
    /// Whether RLM runs self-grade (via the verify rubric) to label
    /// trajectories. The verify sub-call itself is still opt-in per run.
    #[serde(default = "default_true")]
    pub self_grade_enabled: bool,
}

impl Default for A2aRlmConfig {
    fn default() -> Self {
        Self {
            max_depth: default_rlm_max_depth(),
            max_budget: default_rlm_max_budget(),
            neighbor_k: default_rlm_neighbor_k(),
            explore_epsilon: default_rlm_explore_epsilon(),
            self_grade_enabled: true,
        }
    }
}

fn default_rlm_max_depth() -> u32 {
    4
}
fn default_rlm_max_budget() -> u32 {
    64
}
fn default_rlm_neighbor_k() -> usize {
    5
}
fn default_rlm_explore_epsilon() -> f32 {
    0.15
}

/// Disk-backed PersistentARTrieChar fuzzy-index configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FuzzyConfig {
    /// Directory under which the per-project trie files live.
    /// Default: `$XDG_STATE_HOME/pgmcp/` (falls back to `~/.local/state/pgmcp/`
    /// when XDG is unset). Per-kind trie files land at
    /// `$data_dir/fuzzy/{kind}/{project_slug}/{kind}.artrie`.
    #[serde(default = "default_fuzzy_data_dir")]
    pub data_dir: std::path::PathBuf,
    /// Soft cap on per-trie on-disk usage in bytes (default 5 GiB), and the
    /// master switch for heap eviction. `cron::fuzzy_sync`'s `disk_guard`
    /// measures each trie file's on-disk size after a rebuild and logs a warning
    /// (bumping `fuzzy_disk_cap_exceeded`) when it exceeds this cap (advisory —
    /// on-disk size is not shrunk online). Separately, when this is > 0 each
    /// persistent trie is opened with heap eviction enabled
    /// (`FuzzyConfig::eviction_config`): under system memory pressure the
    /// libdictenstein eviction coordinator reclaims in-memory node boxes
    /// (swizzling them to their on-disk locations), bounding RAM independent of
    /// on-disk size. Set to 0 to disable both the disk-cap warning and heap
    /// eviction.
    #[serde(default = "default_fuzzy_max_disk_bytes")]
    pub max_disk_bytes: u64,

    /// P13.3 cost-model knobs. Tunable per-deployment but defaults
    /// reflect liblevenshtein's published articulatory-feature
    /// weights and pgmcp's empirical "fold near-name symbols
    /// together" threshold.
    /// Articulatory voicing-swap cost (`p`↔`b`). Default 0.1.
    #[serde(default = "default_articulatory_voicing_weight")]
    pub articulatory_voicing_weight: f64,
    /// Articulatory place-change step (`p`→`t`→`k`). Default 0.15.
    #[serde(default = "default_articulatory_place_step")]
    pub articulatory_place_step: f64,
    /// Articulatory manner-change default cost. Default 0.5.
    #[serde(default = "default_articulatory_manner_default")]
    pub articulatory_manner_default: f64,
    /// Multiplier on `PhoneticCandidate.phonetic_cost` in WFST
    /// lattice scoring. Default 1.0.
    #[serde(default = "default_phonetic_cost_weight")]
    pub phonetic_cost_weight: f64,
    /// Cap on `edit_distance + phonetic_cost` for candidate
    /// acceptance. Default 3.0.
    #[serde(default = "default_phonetic_max_total_cost")]
    pub phonetic_max_total_cost: f64,
    /// `tool_find_similar_modules` / `tool_find_duplicates` fold
    /// near-name symbols whose articulatory distance ≤ this value
    /// before computing module similarity. Default 2.0 — empirically
    /// covers typical rename / typo cases ("receive" ↔ "recieve",
    /// snake_case ↔ camelCase) while excluding unrelated names.
    #[serde(default = "default_phonetic_merge_threshold")]
    pub phonetic_merge_threshold: f64,
}

impl FuzzyConfig {
    /// Build the libdictenstein [`EvictionConfig`] for the persistent fuzzy
    /// tries. Heap eviction (reclaiming in-memory node boxes under system memory
    /// pressure) is enabled when `max_disk_bytes > 0`, disabled otherwise. This
    /// is distinct from the on-disk `max_disk_bytes` advisory enforced by
    /// `crate::fuzzy::disk_guard`.
    pub fn eviction_config(&self) -> libdictenstein::persistent_artrie::eviction::EvictionConfig {
        use libdictenstein::persistent_artrie::eviction::EvictionConfig;
        if self.max_disk_bytes > 0 {
            EvictionConfig::default()
        } else {
            EvictionConfig::disabled()
        }
    }

    /// Build the per-dimension articulatory feature weights from the `[fuzzy]`
    /// knobs. The three consonant dimensions (voicing/place/manner) are
    /// overridable; vowel dimensions + the manner-table scale stay at
    /// liblevenshtein's built-in defaults.
    pub fn articulatory_weights(
        &self,
    ) -> liblevenshtein::phonetic::feature_distance::FeatureDistanceWeights {
        liblevenshtein::phonetic::feature_distance::FeatureDistanceWeights {
            voicing: self.articulatory_voicing_weight,
            place_step: self.articulatory_place_step,
            manner_default: self.articulatory_manner_default,
            ..liblevenshtein::phonetic::feature_distance::FeatureDistanceWeights::default()
        }
    }
}

impl Default for FuzzyConfig {
    fn default() -> Self {
        Self {
            data_dir: default_fuzzy_data_dir(),
            max_disk_bytes: default_fuzzy_max_disk_bytes(),
            articulatory_voicing_weight: default_articulatory_voicing_weight(),
            articulatory_place_step: default_articulatory_place_step(),
            articulatory_manner_default: default_articulatory_manner_default(),
            phonetic_cost_weight: default_phonetic_cost_weight(),
            phonetic_max_total_cost: default_phonetic_max_total_cost(),
            phonetic_merge_threshold: default_phonetic_merge_threshold(),
        }
    }
}

fn default_articulatory_voicing_weight() -> f64 {
    0.1
}
fn default_articulatory_place_step() -> f64 {
    0.15
}
fn default_articulatory_manner_default() -> f64 {
    0.5
}
fn default_phonetic_cost_weight() -> f64 {
    1.0
}
fn default_phonetic_max_total_cost() -> f64 {
    3.0
}
fn default_phonetic_merge_threshold() -> f64 {
    2.0
}

fn default_fuzzy_data_dir() -> std::path::PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_STATE_HOME") {
        let mut p = std::path::PathBuf::from(xdg);
        p.push("pgmcp");
        return p;
    }
    if let Some(home) = std::env::var_os("HOME") {
        let mut p = std::path::PathBuf::from(home);
        p.push(".local/state/pgmcp");
        return p;
    }
    std::path::PathBuf::from("/var/state/pgmcp")
}

fn default_fuzzy_max_disk_bytes() -> u64 {
    5 * 1024 * 1024 * 1024
}

/// `[api]` — REST API / RAG-hook tuning. `/api/search` (consumed by the
/// UserPromptSubmit hook) always fuses dense + BM25 via RRF; these knobs gate
/// the optional cross-encoder rerank stage, which loads the BGE-reranker model
/// resident in the API state. That model is mutually exclusive in VRAM with the
/// Qwen3 extractor (Phase-11 hardware budget), so reranking is off by default
/// and toggling it requires a daemon restart to (un)load the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiConfig {
    /// Cross-encoder rerank the hybrid `/api/search` results (loads
    /// BGE-reranker-v2-m3). Default false.
    #[serde(default = "default_rerank_hook")]
    pub rerank_hook: bool,
    /// Number of fused candidates to rerank when `rerank_hook` is on; the
    /// response still returns only the request's `limit`. Default 30.
    #[serde(default = "default_rerank_candidates")]
    pub rerank_candidates: i32,
    /// ColBERT late-interaction (MaxSim) rerank of the hybrid `/api/search`
    /// results (Phase 2.5). Recomputes the query + candidate per-token matrices
    /// with the BGE-M3 ColBERT head and reorders by MaxSim. Unlike the
    /// cross-encoder, this reuses the resident embedding model (no extra VRAM),
    /// so it is cheap to leave on. Applied before the cross-encoder when both
    /// are enabled. Default false.
    #[serde(default = "default_colbert_rerank")]
    pub colbert_rerank: bool,
    /// Number of fused candidates to ColBERT-rerank when `colbert_rerank` is on.
    /// Default 50 (wider than the cross-encoder net, since MaxSim is cheap).
    #[serde(default = "default_colbert_candidates")]
    pub colbert_candidates: i32,
    /// MMR diversity λ for the final `/api/search` selection (Phase 4.2):
    /// `λ·relevance − (1−λ)·max-similarity-to-picked`. 0.0 = disabled (pure
    /// relevance order); ~0.7 trades a little relevance for de-duplicating
    /// near-identical chunks in the tiny context budget. Default 0.0.
    #[serde(default = "default_mmr_lambda")]
    pub mmr_lambda: f64,
    /// Recency half-life in days for the `/api/search` recency prior (Phase 4.2):
    /// a candidate's score is multiplied by `0.5^(age/half_life)` using its
    /// `blame_date`. 0.0 = disabled. Default 0.0.
    #[serde(default = "default_recency_half_life_days")]
    pub recency_half_life_days: f64,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            rerank_hook: default_rerank_hook(),
            rerank_candidates: default_rerank_candidates(),
            colbert_rerank: default_colbert_rerank(),
            colbert_candidates: default_colbert_candidates(),
            mmr_lambda: default_mmr_lambda(),
            recency_half_life_days: default_recency_half_life_days(),
        }
    }
}

fn default_rerank_hook() -> bool {
    false
}
fn default_rerank_candidates() -> i32 {
    30
}
fn default_colbert_rerank() -> bool {
    false
}
fn default_colbert_candidates() -> i32 {
    50
}
fn default_mmr_lambda() -> f64 {
    0.0
}
fn default_recency_half_life_days() -> f64 {
    0.0
}

/// Memory-server configuration. Holds Phase 4+ knobs grouped under
/// `[memory.*]` in the TOML.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MemoryConfig {
    #[serde(default)]
    pub extractor: MemoryExtractorConfig,
    #[serde(default)]
    pub reflection: MemoryReflectionConfig,
    #[serde(default)]
    pub retention: MemoryRetentionConfig,
    #[serde(default)]
    pub graph_rag: MemoryGraphRagConfig,
    #[serde(default)]
    pub eval: MemoryEvalConfig,
    #[serde(default)]
    pub latent_pipeline: MemoryLatentPipelineConfig,
    #[serde(default)]
    pub concepts: MemoryConceptsConfig,
}

/// `[memory.latent_pipeline]` — Phase 11 RecursiveLink hand-off
/// between same-backbone pipeline stages. Default `Disabled` per the
/// plan §11.3: the pipeline is opt-in once the operator has (a) the
/// hardware budget and (b) a trained RecursiveLink weights file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryLatentPipelineConfig {
    /// `"qwen3-rlv1"` or `"disabled"`. Default disabled.
    #[serde(default = "default_latent_backend")]
    pub backend: String,
    /// Path to the trained RecursiveLink safetensors file.
    /// Recommended: `models/recursive_link_qwen3_8b.safetensors`.
    #[serde(default = "default_latent_link_path")]
    pub link_weights_path: std::path::PathBuf,
    /// Signature stamped on the weights — bump when retraining with a
    /// new prompt template or backbone variant. Stored alongside
    /// `latent_pipeline_active` in `pgmcp_metadata`.
    #[serde(default = "default_latent_link_signature")]
    pub link_signature: String,
    /// Auto-downgrade threshold: when the daily quality validator
    /// detects `(text_score − latent_score) > quality_regression_threshold`
    /// over a `regression_window` days, the dispatcher demotes back
    /// to the text path. Default 0.05 — a 5-pp absolute quality
    /// regression triggers the downgrade.
    #[serde(default = "default_latent_quality_threshold")]
    pub quality_regression_threshold: f32,
    /// Days of A/B comparison data the validator looks at when deciding.
    #[serde(default = "default_latent_regression_window")]
    pub regression_window_days: i64,
    /// When `true`, the dispatcher attempts a short forward-pass on
    /// startup to confirm VRAM headroom; failure → demote to text.
    #[serde(default = "default_true")]
    pub vram_probe_at_startup: bool,
    #[serde(default)]
    pub train: MemoryLatentTrainConfig,
}

impl Default for MemoryLatentPipelineConfig {
    fn default() -> Self {
        Self {
            backend: default_latent_backend(),
            link_weights_path: default_latent_link_path(),
            link_signature: default_latent_link_signature(),
            quality_regression_threshold: default_latent_quality_threshold(),
            regression_window_days: default_latent_regression_window(),
            vram_probe_at_startup: true,
            train: MemoryLatentTrainConfig::default(),
        }
    }
}

fn default_latent_backend() -> String {
    "disabled".into()
}
fn default_latent_link_path() -> std::path::PathBuf {
    std::path::PathBuf::from("models/recursive_link_qwen3_8b.safetensors")
}
fn default_latent_link_signature() -> String {
    "rlv1".into()
}
fn default_latent_quality_threshold() -> f32 {
    0.05
}
fn default_latent_regression_window() -> i64 {
    7
}

/// `[memory.latent_pipeline.train]` — one-shot trainer settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryLatentTrainConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_latent_train_samples")]
    pub samples_from_session_prompts: usize,
    #[serde(default = "default_latent_train_epochs")]
    pub epochs: usize,
    #[serde(default = "default_latent_train_batch")]
    pub batch_size: usize,
    #[serde(default = "default_latent_train_lr")]
    pub learning_rate: f64,
    #[serde(default = "default_latent_train_seqcap")]
    pub seq_len_cap: usize,
    /// Output path for the trained safetensors file.
    #[serde(default = "default_latent_link_path")]
    pub output_path: std::path::PathBuf,
}

impl Default for MemoryLatentTrainConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            samples_from_session_prompts: default_latent_train_samples(),
            epochs: default_latent_train_epochs(),
            batch_size: default_latent_train_batch(),
            learning_rate: default_latent_train_lr(),
            seq_len_cap: default_latent_train_seqcap(),
            output_path: default_latent_link_path(),
        }
    }
}

fn default_latent_train_samples() -> usize {
    10_000
}
fn default_latent_train_epochs() -> usize {
    3
}
fn default_latent_train_batch() -> usize {
    1
}
fn default_latent_train_lr() -> f64 {
    5e-4
}
fn default_latent_train_seqcap() -> usize {
    1024
}

/// `[memory.eval]` — Phase 9 internal eval harness. The MCP-visible
/// scenarios live in `pgmcp-testing/tests/memory_eval.rs` and run as
/// part of `cargo test` / `scripts/verify.sh`. The cron variant
/// additionally records bi-temporal + provenance invariants into
/// `pgmcp_metadata` on a schedule, so a long-running daemon can detect
/// drift between deploys.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryEvalConfig {
    /// When `false` (default) the periodic invariant scan is skipped
    /// entirely. The integration test suite is always built.
    #[serde(default)]
    pub cron_enabled: bool,
    #[serde(default = "default_memory_eval_interval_secs")]
    pub cron_interval_secs: u64,
    /// Hard cap on rows examined per invariant pass. Keeps the scan
    /// O(N) bounded even on a million-row memory graph.
    #[serde(default = "default_memory_eval_row_cap")]
    pub row_cap: i64,
}

impl Default for MemoryEvalConfig {
    fn default() -> Self {
        Self {
            cron_enabled: false,
            cron_interval_secs: default_memory_eval_interval_secs(),
            row_cap: default_memory_eval_row_cap(),
        }
    }
}

fn default_memory_eval_interval_secs() -> u64 {
    86400
}

fn default_memory_eval_row_cap() -> i64 {
    50_000
}

/// `[memory.extractor]` — LLM-driven salience extraction (Phase 4).
/// Default backend is `disabled` so a stock pgmcp install does not
/// touch the LLM path until the operator opts in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryExtractorConfig {
    /// One of: `qwen3-8b`, `qwen3-4b`, `cloud`, `disabled`.
    #[serde(default = "default_extractor_backend")]
    pub backend: String,
    /// Stage-B debounce per session, in seconds. Stops a flurry of
    /// quick prompts from spamming the GPU.
    #[serde(default = "default_extractor_debounce_secs")]
    pub inline_debounce_secs: u64,
    /// LLM-judged importance threshold for auto-promotion into
    /// `memory_*`. Facts below the threshold are emitted but stamped
    /// with a lower importance (the entity row's `importance` column
    /// reflects the LLM's score directly).
    #[serde(default = "default_extractor_auto_promote_threshold")]
    pub auto_promote_threshold: f32,
    /// Schema-validation strictness: `"strict"` rejects any parse
    /// failure (default); `"lenient"` keeps best-effort parses.
    #[serde(default = "default_extractor_schema_validation")]
    pub schema_validation: String,
}

impl Default for MemoryExtractorConfig {
    fn default() -> Self {
        Self {
            backend: default_extractor_backend(),
            inline_debounce_secs: default_extractor_debounce_secs(),
            auto_promote_threshold: default_extractor_auto_promote_threshold(),
            schema_validation: default_extractor_schema_validation(),
        }
    }
}

fn default_extractor_backend() -> String {
    "disabled".into()
}
fn default_extractor_debounce_secs() -> u64 {
    30
}
fn default_extractor_auto_promote_threshold() -> f32 {
    0.6
}
fn default_extractor_schema_validation() -> String {
    "strict".into()
}

/// `[memory.reflection]` — agent-driven + cron reflection (Phase 5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryReflectionConfig {
    /// The MCP tool `memory_reflect` is always wired; this flag controls
    /// whether the daemon refuses agent calls (off when the operator
    /// wants to avoid LLM spend even with the tool present).
    #[serde(default = "default_true")]
    pub agent_enabled: bool,
    /// Whether the periodic `memory-reflect` cron runs.
    #[serde(default)]
    pub cron_enabled: bool,
    #[serde(default = "default_reflection_cron_interval")]
    pub cron_interval_secs: u64,
    /// Don't reflect on a scope that has fewer than this many new
    /// observations since the last reflection — avoid wasting calls.
    #[serde(default = "default_reflection_min_new")]
    pub min_new_observations: i64,
    /// Max observations included as grounding context for one
    /// reflection call. Bounded by the prompt size budget.
    #[serde(default = "default_reflection_window")]
    pub max_observations: i64,
}

impl Default for MemoryReflectionConfig {
    fn default() -> Self {
        Self {
            agent_enabled: true,
            cron_enabled: false,
            cron_interval_secs: default_reflection_cron_interval(),
            min_new_observations: default_reflection_min_new(),
            max_observations: default_reflection_window(),
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_reflection_cron_interval() -> u64 {
    86400
}
fn default_reflection_min_new() -> i64 {
    50
}
fn default_reflection_window() -> i64 {
    200
}

/// `[memory.concepts]` — Stage-4 auto-population concept layer. Off by default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryConceptsConfig {
    /// Whether the periodic `memory-concept-extract` cron runs.
    #[serde(default)]
    pub cron_enabled: bool,
    #[serde(default = "default_concepts_cron_interval")]
    pub cron_interval_secs: u64,
    /// Minimum member chunks for a topic to seed a concept entity.
    #[serde(default = "default_concepts_min_chunks")]
    pub min_chunks_per_topic: i64,
    /// Max concepts seeded / extracted per run.
    #[serde(default = "default_concepts_max_per_run")]
    pub max_concepts_per_run: i64,
    /// Whether the LLM-emergent concept pass runs (also requires a configured
    /// `[memory.extractor]` backend).
    #[serde(default)]
    pub llm_enabled: bool,
}

impl Default for MemoryConceptsConfig {
    fn default() -> Self {
        Self {
            cron_enabled: false,
            cron_interval_secs: default_concepts_cron_interval(),
            min_chunks_per_topic: default_concepts_min_chunks(),
            max_concepts_per_run: default_concepts_max_per_run(),
            llm_enabled: false,
        }
    }
}

fn default_concepts_cron_interval() -> u64 {
    86400
}
fn default_concepts_min_chunks() -> i64 {
    5
}
fn default_concepts_max_per_run() -> i64 {
    200
}

/// `[cron.trajectory_similarity]` — Stage-5c MSM `evolves_like` edges. Off by default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrajectorySimilarityConfig {
    #[serde(default)]
    pub cron_enabled: bool,
    #[serde(default = "default_traj_interval")]
    pub cron_interval_secs: u64,
    /// Minimum trajectory samples for a record to participate.
    #[serde(default = "default_traj_min_points")]
    pub min_points: i64,
    /// Cap on trajectories per node type (bounds the O(n²) k-NN).
    #[serde(default = "default_traj_max_per_type")]
    pub max_per_type: i64,
    /// Nearest neighbors retained per record.
    #[serde(default = "default_traj_k")]
    pub k_neighbors: i64,
    /// Max MSM distance for an edge (large ⇒ effectively pure k-NN).
    #[serde(default = "default_traj_max_distance")]
    pub max_distance: f64,
    /// MSM split/merge cost `c` (Stefan et al. recommend [0.01, 1.0]).
    #[serde(default = "default_traj_msm_c")]
    pub msm_c: f64,
}

impl Default for TrajectorySimilarityConfig {
    fn default() -> Self {
        Self {
            cron_enabled: false,
            cron_interval_secs: default_traj_interval(),
            min_points: default_traj_min_points(),
            max_per_type: default_traj_max_per_type(),
            k_neighbors: default_traj_k(),
            max_distance: default_traj_max_distance(),
            msm_c: default_traj_msm_c(),
        }
    }
}

fn default_traj_interval() -> u64 {
    21600
}
fn default_traj_min_points() -> i64 {
    3
}
fn default_traj_max_per_type() -> i64 {
    500
}
fn default_traj_k() -> i64 {
    5
}
fn default_traj_max_distance() -> f64 {
    1.0e9
}
fn default_traj_msm_c() -> f64 {
    0.1
}

/// `[memory.retention]` — Phase 8 eviction policy. Stub config now so
/// the TOML accepts the section even though the cron lands in Phase 8.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRetentionConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_retention_window_days")]
    pub window_days: i64,
    #[serde(default = "default_retention_importance")]
    pub importance_threshold: f32,
}

impl Default for MemoryRetentionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            window_days: default_retention_window_days(),
            importance_threshold: default_retention_importance(),
        }
    }
}

fn default_retention_window_days() -> i64 {
    90
}
fn default_retention_importance() -> f32 {
    0.3
}

/// `[memory.graph_rag]` — Phase 6.3–6.5 graph retrieval gating.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryGraphRagConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_graph_rag_max_latency")]
    pub max_latency_ms: i64,
    #[serde(default = "default_graph_rag_path_max_hops")]
    pub path_search_default_max_hops: i32,
    #[serde(default = "default_graph_rag_prune_jaccard")]
    pub path_search_prune_jaccard: f32,
}

impl Default for MemoryGraphRagConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_latency_ms: default_graph_rag_max_latency(),
            path_search_default_max_hops: default_graph_rag_path_max_hops(),
            path_search_prune_jaccard: default_graph_rag_prune_jaccard(),
        }
    }
}

fn default_graph_rag_max_latency() -> i64 {
    500
}
fn default_graph_rag_path_max_hops() -> i32 {
    3
}
fn default_graph_rag_prune_jaccard() -> f32 {
    0.7
}

/// Process-level resource budgets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SystemConfig {
    /// Aggregate RSS budget in MiB. The pool monitors use this as the
    /// `rss_pressure_score` denominator; when the daemon's RSS climbs
    /// past 50% of the budget, the hill-climber's RSS term starts
    /// discouraging unparking, and past 100% it actively parks workers.
    ///
    /// `0` (the default) disables RSS sensing — the climber falls back
    /// to the original two-term throughput-vs-queue-depth behavior.
    /// In daemon startup we resolve `0` to 80% of `MemAvailable` at
    /// boot time.
    #[serde(default)]
    pub rss_limit_mib: u64,
}

impl SystemConfig {
    /// Resolve `rss_limit_mib` to bytes. `0` (auto) returns 80% of
    /// `MemAvailable` at the time of the call, or `0` if /proc/meminfo
    /// is unreadable (in which case RSS sensing stays off — safe
    /// default rather than a wrong limit).
    pub fn resolved_rss_limit_bytes(&self) -> u64 {
        if self.rss_limit_mib > 0 {
            return self.rss_limit_mib * 1024 * 1024;
        }
        crate::stats::rss::mem_available_bytes()
            .map(|avail| avail * 4 / 5)
            .unwrap_or(0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    #[serde(default = "default_workspace_paths")]
    pub paths: Vec<String>,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            paths: default_workspace_paths(),
        }
    }
}

fn default_workspace_paths() -> Vec<String> {
    vec![]
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileTypeMapping {
    pub extension: String,
    pub language: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexerConfig {
    #[serde(default = "default_file_types")]
    pub file_types: Vec<FileTypeMapping>,
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
    #[serde(default = "default_max_file_size")]
    pub max_file_size_bytes: u64,
    /// Maximum content-intrinsic indexing-failure attempts (non-UTF-8, document
    /// extraction failure/timeout/OOM) before the scanner stops re-submitting an
    /// *unchanged* file. Editing the file (mtime advances past the last failure)
    /// lifts the bound. See `src/embed/failure_kind.rs` and the `index_failures`
    /// ledger (v42).
    #[serde(default = "default_max_index_retries")]
    pub max_index_retries: u32,
    #[serde(default = "default_exclude_patterns")]
    pub exclude_patterns: Vec<String>,
    /// Source-form preference for the per-directory dedup pass. When a
    /// directory contains multiple files sharing the same stem (e.g.
    /// `invoice.org` + `invoice.tex` + `invoice.pdf`), the scanner enqueues
    /// only the entry whose extension appears earliest in this list.
    /// Extensions not listed are kept unconditionally.
    #[serde(default = "default_source_priority")]
    pub source_priority: Vec<String>,
    /// Per-file source-byte cap for binary document formats (PDF, DOCX,
    /// EPUB, etc.). The default 1 MiB `max_file_size_bytes` is too small
    /// for typical academic PDFs and would Level-1-skip them; document
    /// languages use this separate cap instead.
    #[serde(default = "default_max_document_source_bytes")]
    pub max_document_source_bytes: u64,
    /// Cap on the extracted-text size held in memory per document. The
    /// subprocess extractors stop reading child stdout at this byte count
    /// and set `truncated = true` rather than fail outright.
    #[serde(default = "default_max_extracted_text_bytes")]
    pub max_extracted_text_bytes: usize,
    /// Per-file timeout for the document extraction subprocess
    /// (`pdftotext`, `ps2ascii`, `pandoc`). Past this, the child is
    /// killed and the file is counted as `documents_extraction_timeout`.
    #[serde(default = "default_document_extraction_timeout_secs")]
    pub document_extraction_timeout_secs: u64,
    /// Hard cap on the address-space size (RLIMIT_AS) of any document
    /// extraction subprocess. Default 4 GiB. Guards against runaway
    /// allocators in `pandoc` / `pdftotext` / `ps2ascii` — a 2026-05-13
    /// pandoc invocation grew to 68 GiB RSS on a single input and got
    /// OOM-killed, taking the daemon's logging task with it. Setting to
    /// `0` disables the limit.
    #[serde(default = "default_max_extraction_subprocess_rss_bytes")]
    pub max_extraction_subprocess_rss_bytes: u64,
    /// Master switch for OCR fallback when `pdftotext` produces sparse
    /// text. When `true` (default), scanned/image-only PDFs are rasterized
    /// with `pdftoppm` and passed through `tesseract` per page; cached by
    /// content_hash so re-runs reuse the OCR output.
    #[serde(default = "default_ocr_enabled")]
    pub ocr_enabled: bool,
    /// Per-page character threshold below which OCR is triggered.
    /// Trigger formula: `pdftotext_chars < ocr_min_text_chars_per_page * page_count`.
    /// 200 chars/page admits sparse but real text (cover pages, single-paragraph
    /// figures) while catching mostly-empty pdftotext output from scans.
    #[serde(default = "default_ocr_min_text_chars_per_page")]
    pub ocr_min_text_chars_per_page: usize,
    /// Hard cap on pages OCRed per document. Protects against a 1000-page
    /// scanned PDF burning hours of CPU. Output beyond this is omitted and
    /// `truncated = true` is set on the result.
    #[serde(default = "default_ocr_max_pages")]
    pub ocr_max_pages: usize,
    /// Rasterization DPI passed to `pdftoppm -r`. 300 is the OCR
    /// industry-standard balance between accuracy and tempdir footprint.
    #[serde(default = "default_ocr_dpi")]
    pub ocr_dpi: u32,
    /// Tesseract language traineddata identifiers. `["eng"]` is the
    /// default; `["eng", "fra"]` joins with `+` for multi-language pages.
    #[serde(default = "default_ocr_languages")]
    pub ocr_languages: Vec<String>,
    /// Per-document wall-clock budget for the full OCR run (rasterize +
    /// all pages). When exceeded, the run is cut short, partial text is
    /// returned, and `truncated = true` is set.
    #[serde(default = "default_ocr_total_timeout_secs")]
    pub ocr_total_timeout_secs: u64,
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            file_types: default_file_types(),
            debounce_ms: default_debounce_ms(),
            max_file_size_bytes: default_max_file_size(),
            max_index_retries: default_max_index_retries(),
            exclude_patterns: default_exclude_patterns(),
            source_priority: default_source_priority(),
            max_document_source_bytes: default_max_document_source_bytes(),
            max_extracted_text_bytes: default_max_extracted_text_bytes(),
            document_extraction_timeout_secs: default_document_extraction_timeout_secs(),
            max_extraction_subprocess_rss_bytes: default_max_extraction_subprocess_rss_bytes(),
            ocr_enabled: default_ocr_enabled(),
            ocr_min_text_chars_per_page: default_ocr_min_text_chars_per_page(),
            ocr_max_pages: default_ocr_max_pages(),
            ocr_dpi: default_ocr_dpi(),
            ocr_languages: default_ocr_languages(),
            ocr_total_timeout_secs: default_ocr_total_timeout_secs(),
        }
    }
}

impl IndexerConfig {
    /// Build extension → language lookup map.
    #[allow(dead_code)]
    pub fn extension_map(&self) -> HashMap<String, String> {
        self.file_types
            .iter()
            .map(|ft| (ft.extension.clone(), ft.language.clone()))
            .collect()
    }

    /// Check if an extension is configured for indexing.
    pub fn is_configured_extension(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|ext| self.file_types.iter().any(|ft| ft.extension == ext))
            .unwrap_or(false)
    }

    /// Get the language for a file path, if configured.
    pub fn language_for_path(&self, path: &Path) -> Option<String> {
        path.extension().and_then(|e| e.to_str()).and_then(|ext| {
            self.file_types
                .iter()
                .find(|ft| ft.extension == ext)
                .map(|ft| ft.language.clone())
        })
    }

    /// Language lookup with directory context. Used by the scanner so we
    /// can apply contextual extension rules — currently:
    ///
    /// - `.cfg` files map to `tlaplus` when a sibling `.tla` file exists in
    ///   the same directory (TLA+ TLC config files travel beside their spec
    ///   files). Outside a TLA+ project directory, `.cfg` is too ambiguous
    ///   (nginx/git/postgres/makefile fragments) and is silently dropped,
    ///   matching the pre-existing behaviour.
    ///
    /// `sibling_extensions` is the set of extensions present in the file's
    /// directory (built once per directory by the scanner). Pass an empty
    /// set to disable contextual rules.
    pub fn language_for_path_in_context(
        &self,
        path: &Path,
        sibling_extensions: &HashSet<String>,
    ) -> Option<String> {
        // First try the path-based lookup.
        if let Some(lang) = self.language_for_path(path) {
            return Some(lang);
        }
        // Contextual rule: `.cfg` in a TLA+ project directory.
        let ext = path.extension().and_then(|e| e.to_str())?;
        if ext == "cfg" && sibling_extensions.contains("tla") {
            return Some("tlaplus".into());
        }
        None
    }

    /// Check inclusion with directory context — paired with
    /// `language_for_path_in_context`.
    pub fn is_configured_extension_in_context(
        &self,
        path: &Path,
        sibling_extensions: &HashSet<String>,
    ) -> bool {
        self.language_for_path_in_context(path, sibling_extensions)
            .is_some()
    }
}

fn default_file_types() -> Vec<FileTypeMapping> {
    vec![
        FileTypeMapping {
            extension: "rs".into(),
            language: "rust".into(),
        },
        FileTypeMapping {
            extension: "md".into(),
            language: "markdown".into(),
        },
        FileTypeMapping {
            extension: "metta".into(),
            language: "metta".into(),
        },
        FileTypeMapping {
            extension: "rho".into(),
            language: "rholang".into(),
        },
        FileTypeMapping {
            extension: "js".into(),
            language: "javascript".into(),
        },
        FileTypeMapping {
            extension: "jsx".into(),
            language: "javascript".into(),
        },
        FileTypeMapping {
            extension: "py".into(),
            language: "python".into(),
        },
        FileTypeMapping {
            extension: "pl".into(),
            language: "prolog".into(),
        },
        FileTypeMapping {
            extension: "pro".into(),
            language: "prolog".into(),
        },
        FileTypeMapping {
            extension: "ts".into(),
            language: "typescript".into(),
        },
        // `.tsx` routes to the dedicated TSX backend
        // (`LanguageRegistry::for_language("tsx")` → `TSX_BACKEND`), not the
        // plain TS backend. Existing `.tsx` rows whose `indexed_files.language`
        // is `"typescript"` will keep that value until the next reindex.
        FileTypeMapping {
            extension: "tsx".into(),
            language: "tsx".into(),
        },
        FileTypeMapping {
            extension: "toml".into(),
            language: "toml".into(),
        },
        FileTypeMapping {
            extension: "json".into(),
            language: "json".into(),
        },
        FileTypeMapping {
            extension: "yaml".into(),
            language: "yaml".into(),
        },
        FileTypeMapping {
            extension: "yml".into(),
            language: "yaml".into(),
        },
        FileTypeMapping {
            extension: "sh".into(),
            language: "shell".into(),
        },
        FileTypeMapping {
            extension: "jsonl".into(),
            language: "jsonl".into(),
        },
        // Tier-0e tree-sitter backends — extensions added 2026-05-01 alongside
        // the symbol-extraction cron. Every language string here must
        // correspond to a `Some(...)` from `LanguageRegistry::for_language`.
        FileTypeMapping {
            extension: "java".into(),
            language: "java".into(),
        },
        FileTypeMapping {
            extension: "scala".into(),
            language: "scala".into(),
        },
        FileTypeMapping {
            extension: "c".into(),
            language: "c".into(),
        },
        FileTypeMapping {
            extension: "h".into(),
            language: "c".into(),
        },
        FileTypeMapping {
            extension: "cpp".into(),
            language: "cpp".into(),
        },
        FileTypeMapping {
            extension: "cc".into(),
            language: "cpp".into(),
        },
        FileTypeMapping {
            extension: "cxx".into(),
            language: "cpp".into(),
        },
        FileTypeMapping {
            extension: "hpp".into(),
            language: "cpp".into(),
        },
        FileTypeMapping {
            extension: "hxx".into(),
            language: "cpp".into(),
        },
        FileTypeMapping {
            extension: "clj".into(),
            language: "clojure".into(),
        },
        FileTypeMapping {
            extension: "cljs".into(),
            language: "clojurescript".into(),
        },
        // `.cljc` (reader-conditional / cross-platform Clojure source) routes
        // to the Clojure backend — it shares the `tree-sitter-clojure` grammar
        // and the `clojure` symbol/import/metric passes handle it identically
        // to `.clj`.
        FileTypeMapping {
            extension: "cljc".into(),
            language: "clojure".into(),
        },
        // Document indexing extensions — extraction is routed through
        // `src/indexer/extract/` to system tools (`pdftotext`,
        // `ps2ascii`, `pandoc`). The `language` strings here are
        // deliberately unique from tree-sitter backend names so that
        // `parsing::LanguageRegistry::for_language` returns `None` for
        // them and the symbol-extraction / graph / import crons skip
        // these languages automatically.
        FileTypeMapping {
            extension: "pdf".into(),
            language: "pdf".into(),
        },
        FileTypeMapping {
            extension: "ps".into(),
            language: "postscript".into(),
        },
        FileTypeMapping {
            extension: "eps".into(),
            language: "postscript".into(),
        },
        FileTypeMapping {
            extension: "tex".into(),
            language: "latex".into(),
        },
        FileTypeMapping {
            extension: "latex".into(),
            language: "latex".into(),
        },
        FileTypeMapping {
            extension: "bib".into(),
            language: "bibtex".into(),
        },
        FileTypeMapping {
            extension: "org".into(),
            language: "org".into(),
        },
        FileTypeMapping {
            extension: "rst".into(),
            language: "rst".into(),
        },
        FileTypeMapping {
            extension: "docx".into(),
            language: "docx".into(),
        },
        FileTypeMapping {
            extension: "doc".into(),
            language: "doc".into(),
        },
        FileTypeMapping {
            extension: "rtf".into(),
            language: "rtf".into(),
        },
        FileTypeMapping {
            extension: "odt".into(),
            language: "odt".into(),
        },
        FileTypeMapping {
            extension: "epub".into(),
            language: "epub".into(),
        },
        FileTypeMapping {
            extension: "txt".into(),
            language: "text".into(),
        },
        // ====================================================================
        // Formal-verification source files (post-SOTA addition).
        //
        // Tier-1 search/embedding/FTS support for every entry below; Tier-2
        // tree-sitter symbol extraction lands separately for coq, tlaplus,
        // lean (dedicated backends) and sage (dispatches to PythonBackend).
        // Note: `.cfg` → `tlaplus` is implemented in
        // `language_for_path_in_context` (only when a sibling `.tla` exists)
        // because `.cfg` is too ambiguous for a global mapping.
        // ====================================================================
        FileTypeMapping {
            extension: "v".into(),
            language: "coq".into(),
        }, // Coq / Rocq
        FileTypeMapping {
            extension: "tla".into(),
            language: "tlaplus".into(),
        }, // TLA+ spec
        FileTypeMapping {
            extension: "smt2".into(),
            language: "smt2".into(),
        }, // Z3 / SMT-LIB 2
        FileTypeMapping {
            extension: "smt".into(),
            language: "smt2".into(),
        }, // Z3 / SMT-LIB 2 (legacy)
        FileTypeMapping {
            extension: "lean".into(),
            language: "lean".into(),
        }, // Lean 4
        FileTypeMapping {
            extension: "sage".into(),
            language: "sage".into(),
        }, // Sage Math
        FileTypeMapping {
            extension: "thy".into(),
            language: "isabelle".into(),
        }, // Isabelle/HOL
        FileTypeMapping {
            extension: "agda".into(),
            language: "agda".into(),
        }, // Agda
        FileTypeMapping {
            extension: "lagda".into(),
            language: "agda".into(),
        }, // Literate Agda
        FileTypeMapping {
            extension: "idr".into(),
            language: "idris".into(),
        }, // Idris 2
        FileTypeMapping {
            extension: "lidr".into(),
            language: "idris".into(),
        }, // Literate Idris
        FileTypeMapping {
            extension: "ipkg".into(),
            language: "idris".into(),
        }, // Idris package
        FileTypeMapping {
            extension: "dfy".into(),
            language: "dafny".into(),
        }, // Dafny
        FileTypeMapping {
            extension: "fst".into(),
            language: "fstar".into(),
        }, // F*
        FileTypeMapping {
            extension: "fsti".into(),
            language: "fstar".into(),
        }, // F* interface
        FileTypeMapping {
            extension: "mlw".into(),
            language: "why3".into(),
        }, // Why3
        FileTypeMapping {
            extension: "als".into(),
            language: "alloy".into(),
        }, // Alloy
        FileTypeMapping {
            extension: "pml".into(),
            language: "promela".into(),
        }, // Spin / Promela
        FileTypeMapping {
            extension: "ec".into(),
            language: "easycrypt".into(),
        }, // EasyCrypt
        FileTypeMapping {
            extension: "eca".into(),
            language: "easycrypt".into(),
        }, // EasyCrypt abstract
        FileTypeMapping {
            extension: "spthy".into(),
            language: "tamarin".into(),
        }, // Tamarin Prover
        FileTypeMapping {
            extension: "pvs".into(),
            language: "pvs".into(),
        }, // PVS
        FileTypeMapping {
            extension: "acl2".into(),
            language: "acl2".into(),
        }, // ACL2
        FileTypeMapping {
            extension: "mm".into(),
            language: "metamath".into(),
        }, // Metamath
        FileTypeMapping {
            extension: "cv".into(),
            language: "cryptoverif".into(),
        }, // CryptoVerif
        FileTypeMapping {
            extension: "ocv".into(),
            language: "cryptoverif".into(),
        }, // CryptoVerif oracle
    ]
}

fn default_debounce_ms() -> u64 {
    300
}

fn default_max_file_size() -> u64 {
    1_048_576 // 1 MB
}

fn default_max_index_retries() -> u32 {
    5
}

fn default_exclude_patterns() -> Vec<String> {
    vec![
        "node_modules".into(),
        "target".into(),
        ".git".into(),
        "__pycache__".into(),
        "*.lock".into(),
        // Formal-verification build artifacts.
        "_build".into(),        // Dune / Coq Makefile output
        ".lake".into(),         // Lean 4 / Lake metadata
        "lake-packages".into(), // Lean 4 / Lake package cache
        "*.vo".into(),          // Coq compiled
        "*.vok".into(),
        "*.vos".into(),
        "*.glob".into(),         // Coq globals dump
        "*.agdai".into(),        // Agda interface files
        ".tlaplus-cache".into(), // TLA+ Toolbox cache
        // cargo-fuzz corpora/artifacts: intentionally-malformed inputs (e.g.
        // `latex-parser/fuzz/corpus/parse_no_panic/broken.tex`) that are pure
        // indexing noise and guarantee extraction failures. Matched as a
        // substring, so `fuzz/corpus` is surgical — it does NOT exclude a
        // legitimate `tests/corpus/` (e.g. the rholang-parser semantics corpus).
        "fuzz/corpus".into(),
        "fuzz/artifacts".into(),
        // macOS / Windows filesystem detritus. `__MACOSX` catches both the
        // top-level `__MACOSX/` directories that unzip from macOS-created
        // archives (and contain AppleDouble `._<name>` siblings) and any
        // nested copies. The AppleDouble forks are not valid UTF-8 — they
        // are HFS+ resource-fork metadata in binary form — and were
        // generating 90+ "stream did not contain valid UTF-8" errors per
        // rescan against `.java` and `.py` paths inside them.
        "__MACOSX".into(),
        "Thumbs.db".into(),
    ]
}

/// Hardcoded fallback priority for choosing one form when multiple sibling
/// files share the same `(parent_dir, file_stem)`. Earlier entries are
/// preferred. Overridable via `[indexer] source_priority = [...]` in the
/// global config and via `[indexer] source_priority = [...]` in a
/// per-project `.pgmcp.toml`.
pub const DEFAULT_SOURCE_PRIORITY: &[&str] = &[
    "org", "rst", "md", "tex", "latex", "docx", "epub", "odt", "rtf", "pdf", "ps", "eps", "doc",
];

fn default_source_priority() -> Vec<String> {
    DEFAULT_SOURCE_PRIORITY
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}

fn default_max_document_source_bytes() -> u64 {
    100 * 1024 * 1024 // 100 MiB — covers virtually all academic PDFs and ebooks
}

fn default_max_extracted_text_bytes() -> usize {
    50 * 1024 * 1024 // 50 MiB of post-extraction text
}

fn default_max_extraction_subprocess_rss_bytes() -> u64 {
    4 * 1024 * 1024 * 1024 // 4 GiB
}

fn default_document_extraction_timeout_secs() -> u64 {
    30
}

fn default_ocr_enabled() -> bool {
    true
}

fn default_ocr_min_text_chars_per_page() -> usize {
    200
}

fn default_ocr_max_pages() -> usize {
    50
}

fn default_ocr_dpi() -> u32 {
    300
}

fn default_ocr_languages() -> Vec<String> {
    vec!["eng".to_string()]
}

fn default_ocr_total_timeout_secs() -> u64 {
    1800 // 30 minutes
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DatabaseConfig {
    #[serde(default = "default_db_host")]
    pub host: String,
    #[serde(default = "default_db_port")]
    pub port: u16,
    #[serde(default = "default_db_name")]
    pub name: String,
    #[serde(default = "default_db_user")]
    pub user: String,
    pub password: Option<String>,
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,

    /// Server-side `statement_timeout` applied to every pooled connection.
    /// Stops runaway analytic queries from holding a connection + locks for
    /// hours. Long-running cron queries that legitimately exceed this raise
    /// it via `SET LOCAL statement_timeout` inside their own transaction.
    #[serde(default = "default_statement_timeout_ms")]
    pub statement_timeout_ms: u32,

    /// Per-leg `statement_timeout` (ms) for `hybrid_search`'s BM25/full-text
    /// leg, scoped via `SET LOCAL` inside that query's own transaction. Much
    /// tighter than `statement_timeout_ms` so a cold / write-contended GIN
    /// index makes the leg give up fast and degrade (the tool still returns
    /// the other legs' hits) rather than burning the daemon-wide ceiling.
    #[serde(default = "default_hybrid_text_leg_timeout_ms")]
    pub hybrid_text_leg_timeout_ms: u32,

    /// Server-side `idle_in_transaction_session_timeout`. Caps the window
    /// during which a misbehaving caller can keep a transaction open
    /// without doing work — Postgres terminates the session past this.
    #[serde(default = "default_idle_in_transaction_timeout_ms")]
    pub idle_in_transaction_timeout_ms: u32,

    /// Server-side `lock_timeout`. Caps the time a query waits to acquire
    /// any individual lock. Avoids unbounded wait on a long-running DDL
    /// or vacuum holding a conflicting lock.
    #[serde(default = "default_lock_timeout_ms")]
    pub lock_timeout_ms: u32,

    /// Server-side `client_connection_check_interval` (PostgreSQL ≥ 14; ignored
    /// on older servers). While a backend runs a long query it otherwise never
    /// notices a vanished client, so a daemon that is SIGKILL-ed / OOM-killed /
    /// crashes mid-query leaves an *orphaned backend* holding its locks until
    /// `statement_timeout` fires (minutes). With this set, the backend polls the
    /// client socket every interval and self-terminates once the client is gone,
    /// releasing its locks promptly — so a restarted daemon's startup migrations
    /// no longer collide with the dead instance and abort at `lock_timeout`.
    /// `0` disables the check. This is the safety net for ungraceful death;
    /// graceful shutdown additionally sweeps heavy backends (see src/db/admin.rs).
    #[serde(default = "default_client_connection_check_interval_ms")]
    pub client_connection_check_interval_ms: u32,

    /// `sqlx` pool-level `idle_timeout`. Connections idle longer than
    /// this are returned to the OS, forcing reconnects through natural
    /// churn instead of waiting for a Postgres-restart cliff to surface
    /// stale connections.
    #[serde(default = "default_pool_idle_timeout_secs")]
    pub pool_idle_timeout_secs: u64,

    /// `sqlx` pool-level `max_lifetime`. Connections older than this are
    /// recycled regardless of activity. Bounds long-term resource drift
    /// in pgbouncer/pgpool middle-tier scenarios.
    #[serde(default = "default_pool_max_lifetime_secs")]
    pub pool_max_lifetime_secs: u64,

    /// `sqlx` `test_before_acquire`. Issues `SELECT 1` on each checkout
    /// to confirm the connection is alive; trades one round-trip for the
    /// guarantee that the caller doesn't get a dead connection after
    /// Postgres restarts.
    #[serde(default = "default_test_before_acquire")]
    pub test_before_acquire: bool,

    /// `crate::health` DB-availability prober cadence (seconds). The prober
    /// runs `SELECT 1` on this interval and flips the shared breaker; consumers
    /// short-circuit while down instead of each eating a 10 s `acquire_timeout`.
    #[serde(default = "default_health_probe_interval_secs")]
    pub health_probe_interval_secs: u64,

    /// Per-probe timeout (seconds). Bounds each probe below the 10 s
    /// `acquire_timeout` so a hung pool cannot make the prober sit on the full
    /// timeout every cycle — an elapsed probe is counted as a failure.
    #[serde(default = "default_health_probe_timeout_secs")]
    pub health_probe_timeout_secs: u64,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            host: default_db_host(),
            port: default_db_port(),
            name: default_db_name(),
            user: default_db_user(),
            password: None,
            max_connections: default_max_connections(),
            statement_timeout_ms: default_statement_timeout_ms(),
            hybrid_text_leg_timeout_ms: default_hybrid_text_leg_timeout_ms(),
            idle_in_transaction_timeout_ms: default_idle_in_transaction_timeout_ms(),
            lock_timeout_ms: default_lock_timeout_ms(),
            client_connection_check_interval_ms: default_client_connection_check_interval_ms(),
            pool_idle_timeout_secs: default_pool_idle_timeout_secs(),
            pool_max_lifetime_secs: default_pool_max_lifetime_secs(),
            test_before_acquire: default_test_before_acquire(),
            health_probe_interval_secs: default_health_probe_interval_secs(),
            health_probe_timeout_secs: default_health_probe_timeout_secs(),
        }
    }
}

fn default_health_probe_interval_secs() -> u64 {
    10
}
fn default_health_probe_timeout_secs() -> u64 {
    5
}

/// `[disk_guard]` — pressure-driven disk-space watchdog (src/health/watchdog.rs).
///
/// Complements the interval-driven `target-cleanup` cron: monitors free **bytes
/// and inodes** continuously and, when a watched filesystem crosses a pause
/// floor on either axis, pauses pgmcp's own disk-growing work (indexing + heavy
/// crons) and triggers cleanup out-of-band. Hysteresis (`resume > pause`)
/// prevents flapping. `pause_floor_gb = 0` disables the guard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskGuardConfig {
    #[serde(default = "default_disk_guard_poll_secs")]
    pub poll_interval_secs: u64,
    /// Free GiB below which to log an early warning (0 disables this axis).
    #[serde(default = "default_disk_guard_warn_gb")]
    pub warn_floor_gb: u64,
    /// Free GiB below which to enter pressure (0 disables the whole guard).
    #[serde(default = "default_disk_guard_pause_gb")]
    pub pause_floor_gb: u64,
    /// Free GiB above which to exit pressure (clamped > pause at runtime).
    #[serde(default = "default_disk_guard_resume_gb")]
    pub resume_floor_gb: u64,
    /// Free inodes below which to warn (0 disables the inode warn axis).
    #[serde(default = "default_disk_guard_warn_inodes")]
    pub warn_floor_inodes: u64,
    /// Free inodes below which to enter pressure (0 disables the inode axis).
    #[serde(default = "default_disk_guard_pause_inodes")]
    pub pause_floor_inodes: u64,
    /// Free inodes above which to exit pressure (clamped > pause at runtime).
    #[serde(default = "default_disk_guard_resume_inodes")]
    pub resume_floor_inodes: u64,
    /// Filesystems to watch; empty falls back to `[cron.target_cleanup] roots`,
    /// then `[workspace] paths`, then `/`.
    #[serde(default)]
    pub paths: Vec<String>,
}

impl Default for DiskGuardConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: default_disk_guard_poll_secs(),
            warn_floor_gb: default_disk_guard_warn_gb(),
            pause_floor_gb: default_disk_guard_pause_gb(),
            resume_floor_gb: default_disk_guard_resume_gb(),
            warn_floor_inodes: default_disk_guard_warn_inodes(),
            pause_floor_inodes: default_disk_guard_pause_inodes(),
            resume_floor_inodes: default_disk_guard_resume_inodes(),
            paths: Vec::new(),
        }
    }
}

fn default_disk_guard_poll_secs() -> u64 {
    30
}
fn default_disk_guard_warn_gb() -> u64 {
    20
}
fn default_disk_guard_pause_gb() -> u64 {
    10
}
fn default_disk_guard_resume_gb() -> u64 {
    25
}
fn default_disk_guard_warn_inodes() -> u64 {
    2_000_000
}
fn default_disk_guard_pause_inodes() -> u64 {
    1_000_000
}
fn default_disk_guard_resume_inodes() -> u64 {
    3_000_000
}

/// `[outbox]` — durable ephemeral-event outbox (src/health/outbox.rs).
///
/// Store-and-forward for the fire-and-forget hook ingress (session-observe /
/// client-file-event) while the DB is down; replayed on recovery. Self-limiting:
/// capped at `max_bytes` and refuses to write when its own filesystem is below
/// `self_floor_*` (so the spool cannot become the next ENOSPC).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxConfig {
    #[serde(default = "default_outbox_enabled")]
    pub enabled: bool,
    /// Spool directory; empty resolves to `$XDG_STATE_HOME/pgmcp/outbox` at
    /// runtime. STRONGLY recommend a separate filesystem or tmpfs (`/dev/shm`):
    /// the outage that motivated this was disk-full on the primary filesystem.
    #[serde(default)]
    pub dir: String,
    /// Total spool size cap.
    #[serde(default = "default_outbox_max_bytes")]
    pub max_bytes: u64,
    /// Refuse to spool when the outbox filesystem has fewer than this many GiB
    /// free (0 disables this guard).
    #[serde(default = "default_outbox_self_floor_gb")]
    pub self_floor_gb: u64,
    /// Refuse to spool when the outbox filesystem has fewer than this many free
    /// inodes (0 disables this guard).
    #[serde(default = "default_outbox_self_floor_inodes")]
    pub self_floor_inodes: u64,
    /// Behavior at `max_bytes`: `stop` (drop new) or `drop_oldest` (trim).
    #[serde(default = "default_outbox_on_full")]
    pub on_full: String,
}

impl Default for OutboxConfig {
    fn default() -> Self {
        Self {
            enabled: default_outbox_enabled(),
            dir: String::new(),
            max_bytes: default_outbox_max_bytes(),
            self_floor_gb: default_outbox_self_floor_gb(),
            self_floor_inodes: default_outbox_self_floor_inodes(),
            on_full: default_outbox_on_full(),
        }
    }
}

impl OutboxConfig {
    /// Resolve the spool directory: explicit `dir`, else
    /// `$XDG_STATE_HOME/pgmcp/outbox` (or `$HOME/.local/state/...`).
    pub fn resolved_dir(&self) -> std::path::PathBuf {
        if !self.dir.is_empty() {
            return std::path::PathBuf::from(&self.dir);
        }
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/state"))
            })
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
        base.join("pgmcp").join("outbox")
    }
}

fn default_outbox_enabled() -> bool {
    true
}
fn default_outbox_max_bytes() -> u64 {
    256 * 1024 * 1024
}
fn default_outbox_self_floor_gb() -> u64 {
    2
}
fn default_outbox_self_floor_inodes() -> u64 {
    100_000
}
fn default_outbox_on_full() -> String {
    "stop".to_string()
}

impl DatabaseConfig {
    /// Build the database connection URL.
    pub fn connection_url(&self) -> String {
        let password = self
            .password
            .clone()
            .or_else(|| std::env::var("PGMCP_DB_PASSWORD").ok())
            .unwrap_or_default();

        if password.is_empty() {
            format!(
                "postgres://{}@{}:{}/{}",
                self.user, self.host, self.port, self.name
            )
        } else {
            format!(
                "postgres://{}:{}@{}:{}/{}",
                self.user, password, self.host, self.port, self.name
            )
        }
    }

    /// Build the database connection URL with the password component
    /// replaced by `****`. Safe to log and to surface via the
    /// `pgmcp status` CLI / `/api/status` endpoint. Always returns the
    /// `:****@` form so a redacted URL is visually distinguishable
    /// from a passwordless one.
    pub fn connection_url_redacted(&self) -> String {
        format!(
            "postgres://{}:****@{}:{}/{}",
            self.user, self.host, self.port, self.name
        )
    }
}

fn default_db_host() -> String {
    "localhost".into()
}
fn default_db_port() -> u16 {
    5432
}
fn default_db_name() -> String {
    "pgmcp".into()
}
fn default_db_user() -> String {
    "pgmcp".into()
}
fn default_max_connections() -> u32 {
    40
}
fn default_statement_timeout_ms() -> u32 {
    30_000
}
fn default_hybrid_text_leg_timeout_ms() -> u32 {
    3_000
}
fn default_idle_in_transaction_timeout_ms() -> u32 {
    60_000
}
fn default_lock_timeout_ms() -> u32 {
    5_000
}
fn default_client_connection_check_interval_ms() -> u32 {
    10_000
}
fn default_pool_idle_timeout_secs() -> u64 {
    1_800
}
fn default_pool_max_lifetime_secs() -> u64 {
    7_200
}
fn default_test_before_acquire() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingsConfig {
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_dimensions")]
    pub dimensions: usize,
    #[serde(default = "default_chunk_size")]
    pub chunk_size_lines: usize,
    #[serde(default = "default_chunk_overlap")]
    pub chunk_overlap_lines: usize,
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,
    #[serde(default = "default_embed_pool_size")]
    pub pool_size: usize,
    /// Enable GPU acceleration for embeddings (requires `cuda` feature).
    #[serde(default)]
    pub use_gpu: bool,
    /// Maximum input token sequence length. Inputs that tokenize to more
    /// than this are truncated. BGE-M3 (XLM-RoBERTa-Large) supports up to
    /// 8192 positions; pgmcp caps at 512 by default, which covers the chunk
    /// sizes this indexer produces. Lowering trades long-input accuracy for
    /// transient memory.
    #[serde(default = "default_max_length")]
    pub max_length: usize,
    /// Cap on input texts per single forward pass inside `Embedder::embed`.
    /// BERT self-attention is `O(batch * seq²)`, so unbounded batches OOM
    /// the GPU on files with many chunks. Default 8 keeps peak VRAM well
    /// under 1 GiB per worker at `max_length = 512`.
    #[serde(default = "default_inference_batch_size")]
    pub inference_batch_size: usize,
    /// Maximum number of embedder copies resident on the GPU at once, across
    /// BOTH the pool workers and the embedding-migration cron. The admission
    /// semaphore (`crate::embed::admission`) enforces this so the always-on
    /// pool (`pool_size` copies) plus the migration cron's transient copy
    /// can't exceed the VRAM budget — the cause of the recurring
    /// `CUDA_ERROR_OUT_OF_MEMORY` on an 8 GiB card. The pool workers are the
    /// baseline residents, so the effective allowance for the migration cron
    /// is `max(gpu_max_resident_embedders, pool_size) - pool_size`: run
    /// `pool_size = 1` during an active re-embed to give the migration a slot
    /// under a budget of 2. Only consulted when `use_gpu = true`.
    #[serde(default = "default_gpu_max_resident_embedders")]
    pub gpu_max_resident_embedders: usize,
}

impl Default for EmbeddingsConfig {
    fn default() -> Self {
        Self {
            model: default_model(),
            dimensions: default_dimensions(),
            chunk_size_lines: default_chunk_size(),
            chunk_overlap_lines: default_chunk_overlap(),
            batch_size: default_batch_size(),
            pool_size: default_embed_pool_size(),
            use_gpu: false,
            max_length: default_max_length(),
            inference_batch_size: default_inference_batch_size(),
            gpu_max_resident_embedders: default_gpu_max_resident_embedders(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VectorConfig {
    /// HNSW index `m` parameter: max number of bi-directional links per node.
    /// Higher values improve recall at the cost of memory and index build time.
    #[serde(default = "default_hnsw_m")]
    pub hnsw_m: i32,
    /// HNSW index `ef_construction` parameter: size of the dynamic candidate list
    /// during index construction. Higher values improve recall at the cost of build time.
    #[serde(default = "default_hnsw_ef_construction")]
    pub hnsw_ef_construction: i32,
    /// `ef_search` parameter set at query time: size of the dynamic candidate list
    /// during search. Higher values improve recall at the cost of query latency.
    #[serde(default = "default_ef_search")]
    pub ef_search: i32,
    /// `maintenance_work_mem` used by `SET LOCAL` during HNSW index
    /// builds. PG accepts unit-suffixed values like `"2GB"`,
    /// `"512MB"`. The cluster default (typically 64 MB) is far too
    /// small for HNSW builds on large tables — pgvector spills to a
    /// slow disk-merge path when the in-memory graph exceeds this.
    /// Default: `"2GB"`. See plan F8.
    #[serde(default = "default_hnsw_maintenance_work_mem")]
    pub hnsw_maintenance_work_mem: String,
    /// Per-transaction `statement_timeout` (seconds) used by `SET LOCAL`
    /// during HNSW index builds. The daemon-wide default
    /// (`[database].statement_timeout_ms = 30000`) is appropriate for
    /// query paths but kills large HNSW builds; this knob raises the
    /// ceiling specifically for the build transaction. `0` (the
    /// default) disables the timeout for HNSW builds.
    #[serde(default = "default_hnsw_build_statement_timeout_secs")]
    pub hnsw_build_statement_timeout_secs: u64,
    /// `max_parallel_maintenance_workers` used by `SET LOCAL` during
    /// HNSW index builds. pgvector ≥ 0.6 supports parallel HNSW
    /// build; this knob caps the worker count. Default 4 (matches
    /// the cluster's typical headroom and pgvector's recommendation).
    #[serde(default = "default_hnsw_max_parallel_workers")]
    pub hnsw_max_parallel_workers: u32,
}

impl Default for VectorConfig {
    fn default() -> Self {
        Self {
            hnsw_m: default_hnsw_m(),
            hnsw_ef_construction: default_hnsw_ef_construction(),
            ef_search: default_ef_search(),
            hnsw_maintenance_work_mem: default_hnsw_maintenance_work_mem(),
            hnsw_build_statement_timeout_secs: default_hnsw_build_statement_timeout_secs(),
            hnsw_max_parallel_workers: default_hnsw_max_parallel_workers(),
        }
    }
}

fn default_hnsw_m() -> i32 {
    24
}
fn default_hnsw_ef_construction() -> i32 {
    200
}
fn default_ef_search() -> i32 {
    100
}
fn default_hnsw_maintenance_work_mem() -> String {
    "2GB".to_string()
}
fn default_hnsw_build_statement_timeout_secs() -> u64 {
    0
}
fn default_hnsw_max_parallel_workers() -> u32 {
    4
}

fn default_model() -> String {
    // ADR-005: pgmcp is BGE-M3/1024-only. MiniLM/384 is no longer supported.
    "bge-m3".into()
}
fn default_dimensions() -> usize {
    // Advisory only — the embedder's true output dim follows `model`
    // (`ModelKind::output_dim`); BGE-M3 is 1024.
    1024
}
fn default_chunk_size() -> usize {
    50
}
fn default_chunk_overlap() -> usize {
    10
}
fn default_batch_size() -> usize {
    32
}
fn default_embed_pool_size() -> usize {
    2
}
fn default_max_length() -> usize {
    512
}
fn default_inference_batch_size() -> usize {
    8
}

fn default_gpu_max_resident_embedders() -> usize {
    2
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpConfig {
    #[serde(default = "default_transport")]
    pub transport: String,
    /// Bind address for the Streamable HTTP transport (daemon mode).
    #[serde(default = "default_mcp_host")]
    pub host: String,
    /// Port for the Streamable HTTP transport (daemon mode).
    #[serde(default = "default_mcp_port")]
    pub port: u16,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            transport: default_transport(),
            host: default_mcp_host(),
            port: default_mcp_port(),
        }
    }
}

fn default_transport() -> String {
    "stdio".into()
}
fn default_mcp_host() -> String {
    "127.0.0.1".into()
}
fn default_mcp_port() -> u16 {
    3100
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoggingConfig {
    #[serde(default = "default_log_file")]
    pub file: String,
    #[serde(default = "default_log_level")]
    pub level: String,
    #[serde(default = "default_rotation")]
    pub rotation: String,
    #[serde(default = "default_max_log_files")]
    pub max_log_files: u32,
    /// Output format for the file sink: `"json"` (default), `"compact"`, or `"pretty"`.
    /// Stderr always uses the compact human-readable form regardless of this setting.
    #[serde(default = "default_log_format")]
    pub format: String,
    /// Optional per-target log-level overrides composed into the
    /// `EnvFilter`. `RUST_LOG` (if set) still wins. Example:
    /// `targets = { "pgmcp::mcp::tool" = "debug", "sqlx::query" = "warn" }`.
    #[serde(default)]
    pub targets: std::collections::BTreeMap<String, String>,
    /// Optional separate file path for an MCP-tool-call-only access log.
    /// When set, the daemon writes a second log file containing only
    /// events from the `pgmcp::mcp::tool` tracing target (the
    /// `invoked` / `completed` / `failed` events from
    /// `instrumented_tool_run`). Useful for keeping an nginx-style
    /// access log of tool traffic separate from general daemon logs.
    /// Uses the same rotation policy and `max_log_files` budget as the
    /// main log file. Tilde-expanded.
    #[serde(default)]
    pub access_log: Option<String>,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            file: default_log_file(),
            level: default_log_level(),
            rotation: default_rotation(),
            max_log_files: default_max_log_files(),
            format: default_log_format(),
            targets: std::collections::BTreeMap::new(),
            access_log: None,
        }
    }
}

fn default_log_file() -> String {
    "~/.local/share/pgmcp/pgmcp.log".into()
}
fn default_log_level() -> String {
    "info".into()
}
fn default_rotation() -> String {
    "daily".into()
}
fn default_max_log_files() -> u32 {
    7
}
fn default_log_format() -> String {
    "json".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkPoolConfig {
    #[serde(default = "default_min_threads")]
    pub min_threads: usize,
    #[serde(default)]
    pub max_threads: usize,
    #[serde(default)]
    pub initial_threads: usize,
}

impl Default for WorkPoolConfig {
    fn default() -> Self {
        Self {
            min_threads: default_min_threads(),
            max_threads: 0,
            initial_threads: 0,
        }
    }
}

impl WorkPoolConfig {
    /// Resolve 0 values to actual thread counts.
    pub fn resolved_max_threads(&self) -> usize {
        if self.max_threads == 0 {
            num_cpus::get()
        } else {
            self.max_threads
        }
    }

    pub fn resolved_initial_threads(&self) -> usize {
        if self.initial_threads == 0 {
            self.min_threads
        } else {
            self.initial_threads
        }
    }
}

fn default_min_threads() -> usize {
    2
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CronConfig {
    #[serde(default = "default_stale_cleanup")]
    pub stale_cleanup_interval_secs: u64,
    #[serde(default = "default_integrity_check")]
    pub integrity_check_interval_secs: u64,
    /// Periodic reconcile-backstop: re-walk every workspace with the Level-1
    /// stat-only skip and re-submit only changed/new files, so live-watcher
    /// events that were missed (inotify overflow, editor atomic-save with
    /// rename, daemon-down-during-edit) self-heal within one interval instead of
    /// waiting for a daemon restart. Cheap (one `stat` per file; only genuinely
    /// changed files are read). `0` disables. See `src/cron/index_reconcile.rs`.
    #[serde(default = "default_index_reconcile")]
    pub index_reconcile_interval_secs: u64,
    #[serde(default = "default_stats_aggregation")]
    pub stats_aggregation_interval_secs: u64,
    #[serde(default = "default_db_maintenance")]
    pub db_maintenance_interval_secs: u64,
    #[serde(default = "default_git_history_index")]
    pub git_history_index_interval_secs: u64,
    #[serde(default = "default_similarity_scan_interval")]
    pub similarity_scan_interval_secs: u64,
    /// Interval between `memory-graph-refresh` cron passes (default: 21600 = 6
    /// hours) that refresh the unified knowledge-graph matviews
    /// (`memory_unified_nodes` + `memory_unified_edges`).
    #[serde(default = "default_memory_graph_refresh_interval")]
    pub memory_graph_refresh_interval_secs: u64,
    /// Stage-5c MSM `evolves_like` trajectory-similarity cron settings.
    #[serde(default)]
    pub trajectory_similarity: TrajectorySimilarityConfig,
    #[serde(default = "default_similarity_threshold")]
    pub similarity_threshold: f64,
    #[serde(default = "default_similarity_top_k")]
    pub similarity_top_k: i32,
    /// Interval between within-project semantic-edge materialization passes,
    /// in seconds (default: 21600 = 6 hours). Feeds the graph-analysis cron's
    /// blended PageRank / betweenness / community detection. (Phase 3.1)
    #[serde(default = "default_semantic_edge_interval")]
    pub semantic_edge_interval_secs: u64,
    /// Minimum chunk-level cosine similarity for a semantic edge (default:
    /// 0.75 — deliberately *lower* than the 0.85 clone floor, since community
    /// blending wants topical affinity, not just near-duplicates).
    #[serde(default = "default_semantic_edge_threshold")]
    pub semantic_edge_threshold: f64,
    /// Max target files retained per source file (fan-out cap, default: 10).
    /// Caps near-clique formation that would otherwise wash out modularity.
    #[serde(default = "default_semantic_edge_fanout")]
    pub semantic_edge_fanout: i32,
    /// Per-chunk HNSW neighbors probed during the semantic scan (default: 5).
    #[serde(default = "default_semantic_edge_per_chunk_k")]
    pub semantic_edge_per_chunk_k: i32,
    /// Interval between RAPTOR-over-code summary-tree rebuilds, in seconds
    /// (default: 43200 = 12 hours — the conceptual tree changes slowly).
    /// (graph-roadmap Phase 3.3)
    #[serde(default = "default_code_raptor_interval")]
    pub code_raptor_interval_secs: u64,
    /// Interval between global topic scans (default: 43200 = 12 hours)
    #[serde(default = "default_topic_scan_interval")]
    pub topic_scan_interval_secs: u64,
    /// FCM min_cluster_size used for K estimation heuristic (default: 5)
    #[serde(default = "default_topic_min_cluster_size")]
    pub topic_min_cluster_size: usize,
    /// Explicit number of clusters (None = auto-estimate via sqrt(n / min_cluster_size))
    #[serde(default)]
    pub topic_num_clusters: Option<usize>,
    /// FCM fuzziness exponent m (default: 2.0; m > 1 controls overlap degree)
    #[serde(default = "default_topic_fuzziness")]
    pub topic_fuzziness: f64,
    /// Maximum FCM iterations (default: 100)
    #[serde(default = "default_topic_fcm_max_iters")]
    pub topic_fcm_max_iters: usize,
    /// FCM convergence tolerance on membership matrix (default: 1e-5)
    #[serde(default = "default_topic_fcm_tolerance")]
    pub topic_fcm_tolerance: f64,
    /// Minimum membership degree to store in DB (default: 0.05)
    #[serde(default = "default_topic_membership_threshold")]
    pub topic_membership_threshold: f64,
    /// Number of top keywords per topic from c-TF-IDF (default: 5)
    #[serde(default = "default_topic_label_top_k")]
    pub topic_label_top_k: usize,

    // ── Phase 1 degeneracy gate (src/quality/topic_metrics.rs) ──────────────
    // Thresholds the topic scan consults BEFORE overwriting good topics, so a
    // collapsed model (uniform memberships / one repeated label / corpus-wide
    // smearing) can never again silently replace a healthy one. See ADR on the
    // topic-clustering redesign.
    /// Gate floor for `mean_max_membership` is `factor / K`. Uniform (collapsed)
    /// memberships are exactly `1/K`; require at least `factor ×` that. Default 2.0.
    #[serde(default = "default_topic_min_mean_max_membership_factor")]
    pub topic_min_mean_max_membership_factor: f64,
    /// Minimum `distinct(labels)/n_topics`. Default 0.30.
    #[serde(default = "default_topic_min_distinct_label_ratio")]
    pub topic_min_distinct_label_ratio: f64,
    /// Maximum mean topics-per-doc (the v3 per-chunk cap is 4). Default 6.0.
    #[serde(default = "default_topic_max_topics_per_doc")]
    pub topic_max_topics_per_doc: f64,
    /// Maximum single-topic share of all assignments. Default 0.60.
    #[serde(default = "default_topic_max_topic_share")]
    pub topic_max_topic_share: f64,
    /// Minimum fuzzy silhouette to accept. Default -1.0 (disabled; the
    /// membership/label signals are the load-bearing gates).
    #[serde(default = "default_topic_min_fuzzy_silhouette")]
    pub topic_min_fuzzy_silhouette: f64,

    // ── Phase 2 topic-clustering engine selection ───────────────────────────
    /// Which topic-clustering engine to use:
    /// - `"baseline"` — FCM on raw 1024-d embeddings (the path that collapsed);
    /// - `"embedding_pca"` — FCM on PCA-reduced embeddings (breaks the
    ///   curse-of-dimensionality collapse; the embedding-BERTopic track);
    /// - `"embedding_rp"` — FCM on JL-random-projection-reduced embeddings;
    /// - `"graph"` — Leiden/Louvain communities over the fused semantic+import+
    ///   co-change graph (`src/cron/topic_graph.rs`);
    /// - `"embedding_hdbscan"` — HDBSCAN over reduced embeddings (per-project).
    ///
    /// Default `"graph"` — the bake-off (2026-06-13) confirmed it the winner
    /// (non-degenerate, clean 1-topic-per-doc partition, ~10× faster than the FCM
    /// embedding tracks); see `default_topic_clustering_method`.
    #[serde(default = "default_topic_clustering_method")]
    pub topic_clustering_method: String,
    /// Target dimensionality for the embedding-track reducers. Default 30.
    #[serde(default = "default_topic_reduce_dim")]
    pub topic_reduce_dim: usize,
    /// Per-edge-type weights for the graph track `[knn_semantic, import,
    /// co_change]`. Default `[1.0, 1.0, 1.0]` (equal fusion).
    #[serde(default = "default_topic_graph_edge_weights")]
    pub topic_graph_edge_weights: Vec<f64>,
    /// Louvain/Leiden resolution for the graph track. Higher → more, smaller
    /// communities. Default 1.0.
    #[serde(default = "default_topic_graph_resolution")]
    pub topic_graph_resolution: f64,

    // ── Phase 4: LLM topic labeling ─────────────────────────────────────────
    /// Replace each stored topic's deterministic c-TF-IDF label with a
    /// human-readable label from the local qwen3 model. The c-TF-IDF keywords
    /// are always kept as the fallback (used verbatim if the model is
    /// unavailable). Applied in the per-project graph cron (authoritative store);
    /// the on-demand `discover_topics` path stays deterministic for speed.
    /// Default true (honoring "LLM labels for all"); set false to disable.
    #[serde(default = "default_topic_llm_labels")]
    pub topic_llm_labels: bool,
    /// Local LLM backend for topic labels: `qwen3-4b` (default) or `qwen3-8b`.
    /// Any other value disables LLM labeling (deterministic labels kept).
    #[serde(default = "default_topic_llm_backend")]
    pub topic_llm_backend: String,

    /// Interval between graph analysis runs in seconds (default: 7200 = 2 hours)
    #[serde(default = "default_graph_analysis_interval")]
    pub graph_analysis_interval_secs: u64,

    /// Interval between symbol-extraction (Tier-0e) runs in seconds (default: 7200 = 2 hours).
    /// The cron runs the per-language `LanguageBackend` impls across the indexed corpus and
    /// persists into `file_symbols` + `symbol_references`. Steady-state cost is bounded by the
    /// per-project `symbol_extraction_last_run:<id>` watermark — only files modified since the
    /// last run are re-extracted.
    #[serde(default = "default_symbol_extraction_interval")]
    pub symbol_extraction_interval_secs: u64,

    /// Interval between fuzzy-index refreshes in seconds (default:
    /// 1800 = 30 min). The `cron::fuzzy_sync` job rebuilds the
    /// `PersistentARTrieChar` indexes (symbols, paths, commits,
    /// durable mandates) from PG so the fuzzy MCP tools (Phase 8)
    /// stay current. Plan reference:
    /// `~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md`
    /// Phase 4.
    #[serde(default = "default_fuzzy_sync_interval")]
    pub fuzzy_sync_interval_secs: u64,

    /// Interval between per-project HybridLanguageModel re-training
    /// runs in seconds (default: 43200 = 12 h). Backs Phase 9's third
    /// RRF leg in `tool_hybrid_search`.
    #[serde(default = "default_ngram_lm_train_interval")]
    pub ngram_lm_train_interval_secs: u64,

    /// Interval between hierarchical-agglomerative topic-dendrogram
    /// rebuilds in seconds (default: 43200 = 12 h). Backs Phase 7's
    /// `dendrogram_topic_hierarchy` MCP tool.
    #[serde(default = "default_topic_dendrogram_interval")]
    pub topic_dendrogram_interval_secs: u64,

    /// Interval between BGE-M3 embedding-backfill cron passes in
    /// seconds (default: 0 = disabled). When non-zero, the daemon
    /// drains `file_chunks` and `session_prompts` rows whose
    /// `embedding_v2` column is NULL each tick. See
    /// `docs/memory-server/02-phases.md` Phase 1 and Phase 5 of
    /// `~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md`.
    /// pgmcp is BGE-M3/1024-only (ADR-005): the schema is pinned to
    /// `bge-m3-v1` at migration time, so this cron only backfills any
    /// 1024d columns still left NULL — there is no separate cutover step.
    #[serde(default = "default_embedding_migration_interval")]
    pub embedding_migration_interval_secs: u64,

    /// Interval for the quality-history snapshot cron (seconds). 0 disables.
    /// Default 6h. Snapshots each project's quality GPAs into
    /// `quality_report_history` so the trend/forecast tools + digest have a
    /// trajectory rather than a single point.
    #[serde(default = "default_quality_history_interval")]
    pub quality_history_interval_secs: u64,

    /// Interval for the retrieval-quality drift cron (seconds). 0 disables
    /// (default). When > 0, periodically scores the frozen probe set
    /// (`src/quality/retrieval_drift.rs`) through `semantic_search` and records
    /// `pgmcp_metadata['retrieval_eval_last_report']`, warning below the floor —
    /// the runtime complement to the CI regression gate.
    #[serde(default)]
    pub retrieval_eval_interval_secs: u64,

    /// Project the retrieval-eval cron scores (default `pgmcp`).
    #[serde(default = "default_retrieval_eval_project")]
    pub retrieval_eval_project: String,

    /// Interval for the `topics-size-history` cron (seconds). 0 disables. Cheap:
    /// snapshots each `code_topics` row's `chunk_count` into
    /// `pgmcp_metadata['topics_size_history']` so `topic_trends` has a per-topic
    /// trajectory rather than a single point.
    #[serde(default = "default_topics_size_history_interval")]
    pub topics_size_history_interval_secs: u64,

    /// Interval for the `tool-policy-refresh` cron (seconds). 0 disables (the
    /// adaptive per-client tool surface then stays on whatever snapshot was
    /// loaded at startup). Default 6h. Recomputes each client's default tool set
    /// (a recency-decayed usage-frequency score, not a trained model — see
    /// `docs/design/tool-policy-recency-decay.md`) from `mcp_tool_calls` into
    /// `client_tool_policy` and hot-swaps the snapshot consulted by `list_tools`.
    #[serde(default = "default_tool_policy_interval")]
    pub tool_policy_interval_secs: u64,

    /// `findings-promotion` cron interval (seconds, default 6h). The cron only
    /// acts on projects that opt in via `[tracker] auto_promote_findings = true`
    /// in their `.pgmcp.toml`; a global interval of 0 disables it entirely.
    #[serde(default = "default_findings_promotion_interval")]
    pub findings_promotion_interval_secs: u64,

    /// `concurrency-scan` cron interval (seconds). Runs the lock-order + channel
    /// deadlock analyses, records findings to `concurrency_findings`, and
    /// materializes `lock_order_edges` + health snapshots (Layer 4). Default 0 =
    /// disabled (opt-in; heavier than findings-promotion).
    #[serde(default = "default_concurrency_scan_interval")]
    pub concurrency_scan_interval_secs: u64,

    /// When true, the `concurrency-scan` cron promotes high-severity deadlock
    /// findings to `pending` `bug` work items (never `confirmed` — confirmation
    /// is user-only). Opt-in, default false; independent of
    /// `[tracker] auto_promote_findings`.
    #[serde(default)]
    pub concurrency_auto_promote: bool,

    /// Retention window (days) for `cron_run_history` rows, swept by the
    /// `db-maintenance` light cron. Default 30; `0` keeps history forever.
    /// See `src/cron/history/` and ADR-018.
    #[serde(default = "default_cron_history_retention_days")]
    pub cron_history_retention_days: i64,

    /// Batch size for the embedding-migration cron (default 64).
    #[serde(default = "default_embedding_migration_batch_size")]
    pub embedding_migration_batch_size: usize,

    /// Cap on batches per cron tick (default 32).
    #[serde(default = "default_embedding_migration_max_batches")]
    pub embedding_migration_max_batches: usize,

    // -----------------------------------------------------------------------
    // OOM-fix additions (Phase 1)
    // -----------------------------------------------------------------------
    /// Maximum fraction of /proc/meminfo:MemAvailable that global topic clustering
    /// is allowed to predict using. If prediction exceeds this, fall back to the
    /// per-project emergency path. Default: 0.4 (use at most 40% of available memory).
    #[serde(default = "default_topic_max_mem_fraction")]
    pub topic_max_mem_fraction: f64,

    /// Scratch directory for the mmap-backed data matrix. Default: $XDG_CACHE_HOME/pgmcp
    /// (falls back to /tmp/pgmcp if XDG is unset). Files named `fcm-scratch-<pid>-<ts>.dat`
    /// are created and unlinked automatically.
    #[serde(default)]
    pub topic_scratch_dir: Option<std::path::PathBuf>,

    /// Ready-relative initial delay for git-history-index cron (seconds).
    /// Default 300 = wait 5 minutes after the daemon reaches Ready.
    #[serde(default = "default_ready_delay_git_secs")]
    pub ready_delay_git_secs: u64,

    /// Ready-relative initial delay for similarity-scan cron (seconds).
    /// Default 900 = 15 minutes.
    #[serde(default = "default_ready_delay_similarity_secs")]
    pub ready_delay_similarity_secs: u64,

    /// Ready-relative initial delay for graph-analysis cron (seconds).
    /// Default 1800 = 30 minutes.
    #[serde(default = "default_ready_delay_graph_secs")]
    pub ready_delay_graph_secs: u64,

    /// Ready-relative initial delay for semantic-edges cron (seconds).
    /// Default 1200 = 20 minutes — sequenced between similarity-scan (15m) and
    /// graph-analysis (30m) so semantic edges are materialized before the
    /// graph-analysis pass blends them into centrality/community. (Phase 3.1)
    #[serde(default = "default_ready_delay_semantic_secs")]
    pub ready_delay_semantic_secs: u64,

    /// Ready-relative initial delay for code-raptor cron (seconds).
    /// Default 2400 = 40 minutes — runs after topic-clustering so embeddings
    /// are settled; heavy (CUDA FCM per project). (graph-roadmap Phase 3.3)
    #[serde(default = "default_ready_delay_code_raptor_secs")]
    pub ready_delay_code_raptor_secs: u64,

    /// Ready-relative initial delay for topic-clustering cron (seconds).
    /// Default 3600 = 60 minutes.
    #[serde(default = "default_ready_delay_topic_secs")]
    pub ready_delay_topic_secs: u64,

    /// Ready-relative initial delay for the embedding-migration cron
    /// (seconds). Default 60 = 1 minute. Unlike topic clustering or
    /// graph analysis, the migration cron has nothing to wait for
    /// post-Ready — it just drains rows whose `embedding_v2` column
    /// is NULL. A 1-hour delay (the prior behaviour, inherited by
    /// reusing `ready_delay_topic_secs`) blocked the BGE-M3 cutover
    /// drain for an hour after every daemon restart. See plan
    /// `~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md`
    /// boy-scout follow-up (2026-05-25).
    #[serde(default = "default_ready_delay_embedding_migration_secs")]
    pub ready_delay_embedding_migration_secs: u64,

    /// Ready-relative initial delay for symbol-extraction cron (seconds).
    /// Default 1800 = 30 minutes (matches `ready_delay_graph_secs`).
    #[serde(default = "default_ready_delay_symbol_extraction_secs")]
    pub ready_delay_symbol_extraction_secs: u64,

    /// Interval between function-metrics (SOTA Phase 1) runs in seconds
    /// (default: 7200 = 2 hours). Runs after symbol-extraction; computes
    /// cyclomatic / cognitive / Halstead / NPath / MI per function.
    #[serde(default = "default_function_metrics_interval")]
    pub function_metrics_interval_secs: u64,

    /// Ready-relative initial delay for function-metrics cron (seconds).
    /// Default 2100 = 35 minutes (sequenced after symbol-extraction's 30m).
    #[serde(default = "default_ready_delay_function_metrics_secs")]
    pub ready_delay_function_metrics_secs: u64,

    /// Interval between call-graph (SOTA Phase 1) runs in seconds
    /// (default: 7200 = 2 hours). Materializes symbol-resolved call edges
    /// into `code_graph_edges` and updates `function_metrics.fan_in/fan_out`.
    #[serde(default = "default_call_graph_interval")]
    pub call_graph_interval_secs: u64,

    /// Ready-relative initial delay for call-graph cron (seconds).
    /// Default 2400 = 40 minutes (sequenced after function-metrics's 35m).
    #[serde(default = "default_ready_delay_call_graph_secs")]
    pub ready_delay_call_graph_secs: u64,

    /// GPU FCM precision selector (cuda feature only). Valid values: "fp32",
    /// "fp16", "bf16". Default: "fp16" — mixed precision with fp32 accumulator,
    /// Tensor Cores enabled on Ada Lovelace / Hopper GPUs. Falls back to fp32
    /// cuBLAS SGEMM if the GPU doesn't support the requested precision.
    #[serde(default = "default_gpu_fcm_precision")]
    pub gpu_fcm_precision: String,

    /// Adaptive K selector index (Phase 12). Valid values: "xie_beni"
    /// (default, cheapest), "silhouette" (fuzzy silhouette), "gap"
    /// (Gap statistic, most expensive).
    #[serde(default = "default_topic_k_selector")]
    pub topic_k_selector: String,

    /// Candidate K values for the sweep. Empty = use geometric sweep
    /// around `estimate_k` (K_base · 2^{-2..+2}, clamped [10, 500]).
    #[serde(default)]
    pub topic_k_candidates: Vec<usize>,

    /// Max iterations per short-FCM during the K sweep (default 20).
    #[serde(default = "default_topic_k_sweep_max_iters")]
    pub topic_k_sweep_max_iters: usize,

    /// Subsample size for the K sweep — pass only this many rows of `data`
    /// to the short FCM runs (default 50 000). 0 disables subsampling.
    #[serde(default = "default_topic_k_sweep_subsample")]
    pub topic_k_sweep_subsample: usize,

    /// LMDB path for persistent topic state (Phase 7). None = XDG default
    /// (`$XDG_DATA_HOME/pgmcp/topics.lmdb`).
    #[serde(default)]
    pub topic_lmdb_path: Option<std::path::PathBuf>,

    /// Enable LMDB-backed warm-start. Default true. Set false to always
    /// cold-start via k-means++.
    #[serde(default = "default_topic_lmdb_enabled")]
    pub topic_lmdb_enabled: bool,

    /// n threshold above which `run_global_topic_scan` dispatches to the
    /// online mini-batch FCM (Phase 8). Default 1_000_000.
    #[serde(default = "default_topic_online_n_threshold")]
    pub topic_online_n_threshold: usize,

    /// Mini-batch size for the online FCM (Phase 8). Default 10_000.
    #[serde(default = "default_topic_online_batch_size")]
    pub topic_online_batch_size: usize,

    /// n threshold above which `run_global_topic_scan` uses the mmap-backed
    /// data matrix + streaming c-TF-IDF (Phase 1.2-1.3) instead of loading all
    /// ChunkEmbeddingRow records in one `fetch_all`. Default 50_000. Must be
    /// <= topic_online_n_threshold; above that threshold the online FCM
    /// (Phase 8) takes over.
    #[serde(default = "default_topic_mmap_n_threshold")]
    pub topic_mmap_n_threshold: usize,

    /// Interval between work-item-presence/lease-decay cron sweeps, in seconds
    /// (default: 60). A light job (a couple of bounded UPDATEs): NULLs expired
    /// `work_items.claimed_by` leases (+ `expire` ledger rows) and decays
    /// `agent_presence` active→idle→offline. Backs the A2A collaboration
    /// crash-safety guarantee (a crashed agent's claims become stealable).
    #[serde(default = "default_work_item_presence_interval")]
    pub work_item_presence_interval_secs: u64,

    /// Seconds of inactivity after which an `active` agent_presence row decays
    /// to `idle` (default: 300 = 5 min). Liveness only — does not release leases.
    #[serde(default = "default_work_item_presence_idle")]
    pub work_item_presence_idle_secs: u64,

    /// Seconds of inactivity after which an agent_presence row decays to
    /// `offline` (default: 900 = 15 min). Independent of lease expiry, which is
    /// driven by each claim's own `lease_expires_at`.
    #[serde(default = "default_work_item_presence_offline")]
    pub work_item_presence_offline_secs: u64,

    /// Interval (seconds) of the `orchestration-session-reaper` crash-resume cron
    /// (ADR-009 PAUSE/RESUME). A single bounded UPDATE that auto-pauses every live
    /// (`running`/`resuming`) `orchestration_sessions` row whose work-item lease
    /// has lapsed — i.e. the orchestrator (pi) crashed mid-protocol — so the
    /// session surfaces in `session_checkpoint_list` for another agent to resume.
    /// **OFF by default (0 disables)**, like the presence / csm-validate crons.
    #[serde(default = "default_orchestration_session_reaper_interval")]
    pub orchestration_session_reaper_interval_secs: u64,

    /// Interval (seconds) of the `mcp-client-liveness` sweep: re-checks each
    /// `alive` `mcp_clients` PID via `/proc` (existence + start-time
    /// fingerprint, so a recycled PID counts as exited), refreshes cwd/project
    /// for survivors, and flips dead clients to `exited`. Backs the
    /// `active_clients` tool + the A2A active-agents-by-project view. Default 30.
    #[serde(default = "default_mcp_client_liveness_interval")]
    pub mcp_client_liveness_interval_secs: u64,

    /// Interval (seconds) of the `project-deps-index` cron: re-parses each
    /// project's Cargo manifests into `project_dependencies` (source=cargo),
    /// upserting live cross-project edges and closing vanished ones. Default
    /// 3600 (1h).
    #[serde(default = "default_project_deps_index_interval")]
    pub project_deps_index_interval_secs: u64,

    /// Interval (seconds) of the `git-state-scan` cron: for projects under an
    /// open worktree-coordination request, reads live git state and resolves the
    /// coordination when the dependency is back on its stable branch & clean (the
    /// gatekeeper close-the-loop). Scoped to active coordinations, so cheap.
    /// Default 45.
    #[serde(default = "default_git_state_scan_interval")]
    pub git_state_scan_interval_secs: u64,

    /// `target-cleanup` cron: periodic, safe reclamation of regeneratable Rust
    /// `target/` build artifacts plus a provenance-first sweep of `/tmp` +
    /// `/var/tmp`. Nested so its (many) knobs live under
    /// `[cron.target_cleanup]`. Ships **enabled but dry-run**; see
    /// [`TargetCleanupConfig`] and `src/cron/target_cleanup.rs`.
    #[serde(default)]
    pub target_cleanup: TargetCleanupConfig,
}

impl Default for CronConfig {
    fn default() -> Self {
        Self {
            stale_cleanup_interval_secs: default_stale_cleanup(),
            integrity_check_interval_secs: default_integrity_check(),
            index_reconcile_interval_secs: default_index_reconcile(),
            stats_aggregation_interval_secs: default_stats_aggregation(),
            db_maintenance_interval_secs: default_db_maintenance(),
            git_history_index_interval_secs: default_git_history_index(),
            similarity_scan_interval_secs: default_similarity_scan_interval(),
            memory_graph_refresh_interval_secs: default_memory_graph_refresh_interval(),
            trajectory_similarity: TrajectorySimilarityConfig::default(),
            similarity_threshold: default_similarity_threshold(),
            similarity_top_k: default_similarity_top_k(),
            semantic_edge_interval_secs: default_semantic_edge_interval(),
            semantic_edge_threshold: default_semantic_edge_threshold(),
            semantic_edge_fanout: default_semantic_edge_fanout(),
            semantic_edge_per_chunk_k: default_semantic_edge_per_chunk_k(),
            code_raptor_interval_secs: default_code_raptor_interval(),
            topic_scan_interval_secs: default_topic_scan_interval(),
            topic_min_cluster_size: default_topic_min_cluster_size(),
            topic_num_clusters: None,
            topic_fuzziness: default_topic_fuzziness(),
            topic_fcm_max_iters: default_topic_fcm_max_iters(),
            topic_fcm_tolerance: default_topic_fcm_tolerance(),
            topic_membership_threshold: default_topic_membership_threshold(),
            topic_label_top_k: default_topic_label_top_k(),
            topic_min_mean_max_membership_factor: default_topic_min_mean_max_membership_factor(),
            topic_min_distinct_label_ratio: default_topic_min_distinct_label_ratio(),
            topic_max_topics_per_doc: default_topic_max_topics_per_doc(),
            topic_max_topic_share: default_topic_max_topic_share(),
            topic_min_fuzzy_silhouette: default_topic_min_fuzzy_silhouette(),
            topic_clustering_method: default_topic_clustering_method(),
            topic_reduce_dim: default_topic_reduce_dim(),
            topic_graph_edge_weights: default_topic_graph_edge_weights(),
            topic_graph_resolution: default_topic_graph_resolution(),
            topic_llm_labels: default_topic_llm_labels(),
            topic_llm_backend: default_topic_llm_backend(),
            graph_analysis_interval_secs: default_graph_analysis_interval(),
            symbol_extraction_interval_secs: default_symbol_extraction_interval(),
            fuzzy_sync_interval_secs: default_fuzzy_sync_interval(),
            ngram_lm_train_interval_secs: default_ngram_lm_train_interval(),
            topic_dendrogram_interval_secs: default_topic_dendrogram_interval(),
            embedding_migration_interval_secs: default_embedding_migration_interval(),
            quality_history_interval_secs: default_quality_history_interval(),
            retrieval_eval_interval_secs: 0,
            retrieval_eval_project: default_retrieval_eval_project(),
            topics_size_history_interval_secs: default_topics_size_history_interval(),
            tool_policy_interval_secs: default_tool_policy_interval(),
            findings_promotion_interval_secs: default_findings_promotion_interval(),
            concurrency_scan_interval_secs: default_concurrency_scan_interval(),
            concurrency_auto_promote: false,
            cron_history_retention_days: default_cron_history_retention_days(),
            embedding_migration_batch_size: default_embedding_migration_batch_size(),
            embedding_migration_max_batches: default_embedding_migration_max_batches(),
            topic_max_mem_fraction: default_topic_max_mem_fraction(),
            topic_scratch_dir: None,
            ready_delay_git_secs: default_ready_delay_git_secs(),
            ready_delay_similarity_secs: default_ready_delay_similarity_secs(),
            ready_delay_graph_secs: default_ready_delay_graph_secs(),
            ready_delay_semantic_secs: default_ready_delay_semantic_secs(),
            ready_delay_code_raptor_secs: default_ready_delay_code_raptor_secs(),
            ready_delay_topic_secs: default_ready_delay_topic_secs(),
            ready_delay_embedding_migration_secs: default_ready_delay_embedding_migration_secs(),
            ready_delay_symbol_extraction_secs: default_ready_delay_symbol_extraction_secs(),
            function_metrics_interval_secs: default_function_metrics_interval(),
            ready_delay_function_metrics_secs: default_ready_delay_function_metrics_secs(),
            call_graph_interval_secs: default_call_graph_interval(),
            ready_delay_call_graph_secs: default_ready_delay_call_graph_secs(),
            gpu_fcm_precision: default_gpu_fcm_precision(),
            topic_k_selector: default_topic_k_selector(),
            topic_k_candidates: Vec::new(),
            topic_k_sweep_max_iters: default_topic_k_sweep_max_iters(),
            topic_k_sweep_subsample: default_topic_k_sweep_subsample(),
            topic_lmdb_path: None,
            topic_lmdb_enabled: default_topic_lmdb_enabled(),
            topic_online_n_threshold: default_topic_online_n_threshold(),
            topic_online_batch_size: default_topic_online_batch_size(),
            topic_mmap_n_threshold: default_topic_mmap_n_threshold(),
            work_item_presence_interval_secs: default_work_item_presence_interval(),
            work_item_presence_idle_secs: default_work_item_presence_idle(),
            work_item_presence_offline_secs: default_work_item_presence_offline(),
            orchestration_session_reaper_interval_secs:
                default_orchestration_session_reaper_interval(),
            mcp_client_liveness_interval_secs: default_mcp_client_liveness_interval(),
            project_deps_index_interval_secs: default_project_deps_index_interval(),
            git_state_scan_interval_secs: default_git_state_scan_interval(),
            target_cleanup: TargetCleanupConfig::default(),
        }
    }
}

/// Configuration for the `target-cleanup` cron (`[cron.target_cleanup]`).
///
/// Two phases per run: (1) reclaim regeneratable Rust `target/` build
/// artifacts under the resolved roots, tiered by project staleness; and (2) a
/// provenance-first sweep of `/tmp` + `/var/tmp`. Ships **enabled but
/// `dry_run = true`** — every run writes a manifest of what it *would* delete
/// and removes nothing until an operator sets `dry_run = false`. Build
/// artifacts are recoverable-by-rebuild, and the deletion chokepoint refuses
/// any path not inside a genuine `*/target` (sibling `Cargo.toml`), so
/// unattended operation cannot touch a source file or the running daemon's own
/// binary. Full design: `src/cron/target_cleanup.rs`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TargetCleanupConfig {
    /// Cadence in seconds (default 604800 = weekly). 0 disables the cron.
    #[serde(default = "default_target_cleanup_interval")]
    pub interval_secs: u64,
    /// Log-only when true (default true): a manifest is written but nothing is
    /// deleted. Set false to arm actual removal.
    #[serde(default = "default_target_cleanup_dry_run")]
    pub dry_run: bool,
    /// A project with git/source activity within this many days is "active":
    /// Tier 1 keeps artifacts newer than this and trims older ones (default 14).
    #[serde(default = "default_target_cleanup_active_days")]
    pub active_days: u64,
    /// A project idle longer than this many days is "stale": its whole
    /// `target/` is Tier-2 eligible for a full wipe (default 60).
    #[serde(default = "default_target_cleanup_stale_days")]
    pub stale_days: u64,
    /// Skip any `target/` with a file modified within this many minutes — a
    /// build may be in progress (default 10).
    #[serde(default = "default_target_cleanup_build_quiet_mins")]
    pub build_quiet_mins: u64,
    /// When > 0, gate the aggressive tiers (1 and 2) on disk pressure: run them
    /// only when free space on the roots' filesystem is below this many GiB.
    /// 0 (default) = always run the tiers on schedule ("Moderate").
    #[serde(default)]
    pub free_floor_gb: u64,
    /// Extra directories to bounded-walk for `target/` dirs. Empty (default) =
    /// discover from the indexed `projects` table (every known project's
    /// `<path>/target`). Set to force-include roots the index does not cover.
    #[serde(default)]
    pub roots: Vec<String>,
    /// Project roots that must never be touched (in addition to the running
    /// daemon's own project, which is always protected).
    #[serde(default)]
    pub allowlist: Vec<String>,
    /// Also sweep `/tmp` + `/var/tmp` (default true), provenance-first.
    #[serde(default = "default_target_cleanup_sweep_tmp")]
    pub sweep_tmp: bool,
    /// Temp directories swept when `sweep_tmp` is true.
    #[serde(default = "default_target_cleanup_tmp_dirs")]
    pub tmp_dirs: Vec<String>,
    /// After the agent that created/last-touched an attributed tmp file is
    /// gone, wait this many seconds before deleting it (default 3600).
    #[serde(default = "default_target_cleanup_tmp_attributed_grace")]
    pub tmp_attributed_grace_secs: u64,
    /// A session whose `last_seen` is older than this many seconds counts as
    /// gone (hook-sourced provenance liveness; default 7200).
    #[serde(default = "default_target_cleanup_tmp_session_grace")]
    pub tmp_session_grace_secs: u64,
    /// Age threshold (days, by mtime AND atime) for deleting an *unattributed*
    /// file under `/tmp` (default 10, matching systemd-tmpfiles).
    #[serde(default = "default_target_cleanup_tmp_unattributed_age_days")]
    pub tmp_unattributed_age_days: u64,
    /// Age threshold (days) for an *unattributed* file under `/var/tmp`
    /// (default 30, matching systemd-tmpfiles).
    #[serde(default = "default_target_cleanup_tmp_unattributed_var_age_days")]
    pub tmp_unattributed_var_age_days: u64,
}

impl Default for TargetCleanupConfig {
    fn default() -> Self {
        Self {
            interval_secs: default_target_cleanup_interval(),
            dry_run: default_target_cleanup_dry_run(),
            active_days: default_target_cleanup_active_days(),
            stale_days: default_target_cleanup_stale_days(),
            build_quiet_mins: default_target_cleanup_build_quiet_mins(),
            free_floor_gb: 0,
            roots: Vec::new(),
            allowlist: Vec::new(),
            sweep_tmp: default_target_cleanup_sweep_tmp(),
            tmp_dirs: default_target_cleanup_tmp_dirs(),
            tmp_attributed_grace_secs: default_target_cleanup_tmp_attributed_grace(),
            tmp_session_grace_secs: default_target_cleanup_tmp_session_grace(),
            tmp_unattributed_age_days: default_target_cleanup_tmp_unattributed_age_days(),
            tmp_unattributed_var_age_days: default_target_cleanup_tmp_unattributed_var_age_days(),
        }
    }
}

fn default_target_cleanup_interval() -> u64 {
    604800
}
fn default_target_cleanup_dry_run() -> bool {
    true
}
fn default_target_cleanup_active_days() -> u64 {
    14
}
fn default_target_cleanup_stale_days() -> u64 {
    60
}
fn default_target_cleanup_build_quiet_mins() -> u64 {
    10
}
fn default_target_cleanup_sweep_tmp() -> bool {
    true
}
fn default_target_cleanup_tmp_dirs() -> Vec<String> {
    vec!["/tmp".to_string(), "/var/tmp".to_string()]
}
fn default_target_cleanup_tmp_attributed_grace() -> u64 {
    3600
}
fn default_target_cleanup_tmp_session_grace() -> u64 {
    7200
}
fn default_target_cleanup_tmp_unattributed_age_days() -> u64 {
    10
}
fn default_target_cleanup_tmp_unattributed_var_age_days() -> u64 {
    30
}

fn default_work_item_presence_interval() -> u64 {
    60
}
fn default_mcp_client_liveness_interval() -> u64 {
    30
}
fn default_project_deps_index_interval() -> u64 {
    3600
}
fn default_git_state_scan_interval() -> u64 {
    45
}
fn default_work_item_presence_idle() -> u64 {
    300
}
fn default_work_item_presence_offline() -> u64 {
    900
}
fn default_orchestration_session_reaper_interval() -> u64 {
    // OFF by default — opt-in, like the presence / csm-validate crons.
    0
}

fn default_topic_max_mem_fraction() -> f64 {
    0.4
}
fn default_ready_delay_git_secs() -> u64 {
    300
}
fn default_ready_delay_similarity_secs() -> u64 {
    900
}
fn default_ready_delay_graph_secs() -> u64 {
    1800
}
fn default_ready_delay_semantic_secs() -> u64 {
    1200
}
fn default_ready_delay_topic_secs() -> u64 {
    3600
}
fn default_ready_delay_embedding_migration_secs() -> u64 {
    60
}
fn default_ready_delay_symbol_extraction_secs() -> u64 {
    1800
}
fn default_function_metrics_interval() -> u64 {
    7200
} // 2 hours
fn default_ready_delay_function_metrics_secs() -> u64 {
    2100
} // 35 minutes — sequenced after symbol-extraction's 30m
fn default_call_graph_interval() -> u64 {
    7200
} // 2 hours
fn default_ready_delay_call_graph_secs() -> u64 {
    2400
} // 40 minutes — sequenced after function-metrics's 35m
fn default_gpu_fcm_precision() -> String {
    "fp16".into()
}
fn default_topic_k_selector() -> String {
    "xie_beni".into()
}
fn default_topic_k_sweep_max_iters() -> usize {
    20
}
fn default_topic_k_sweep_subsample() -> usize {
    50_000
}
fn default_topic_lmdb_enabled() -> bool {
    true
}
fn default_topic_online_n_threshold() -> usize {
    1_000_000
}
fn default_topic_online_batch_size() -> usize {
    10_000
}
fn default_topic_mmap_n_threshold() -> usize {
    50_000
}

fn default_stale_cleanup() -> u64 {
    3600
}
fn default_integrity_check() -> u64 {
    86400
}
fn default_index_reconcile() -> u64 {
    1800 // 30 min: bounds worst-case staleness from a missed live event
}
fn default_stats_aggregation() -> u64 {
    60
}
fn default_db_maintenance() -> u64 {
    604_800
}
fn default_git_history_index() -> u64 {
    3600
}
fn default_similarity_scan_interval() -> u64 {
    21600
} // 6 hours
fn default_memory_graph_refresh_interval() -> u64 {
    21600
} // 6 hours
fn default_similarity_threshold() -> f64 {
    0.85
}
fn default_similarity_top_k() -> i32 {
    10
}
fn default_semantic_edge_interval() -> u64 {
    21600
} // 6 hours
fn default_semantic_edge_threshold() -> f64 {
    0.75
}
fn default_semantic_edge_fanout() -> i32 {
    10
}
fn default_semantic_edge_per_chunk_k() -> i32 {
    5
}
fn default_code_raptor_interval() -> u64 {
    43200
} // 12 hours
fn default_ready_delay_code_raptor_secs() -> u64 {
    2400
}
fn default_topic_scan_interval() -> u64 {
    43200
} // 12 hours
fn default_topic_min_cluster_size() -> usize {
    5
}
fn default_topic_fuzziness() -> f64 {
    2.0
}
fn default_topic_fcm_max_iters() -> usize {
    100
}
fn default_topic_fcm_tolerance() -> f64 {
    1e-5
}
fn default_topic_membership_threshold() -> f64 {
    0.05
}
fn default_topic_label_top_k() -> usize {
    5
}
fn default_topic_min_mean_max_membership_factor() -> f64 {
    2.0
}
fn default_topic_min_distinct_label_ratio() -> f64 {
    0.30
}
fn default_topic_max_topics_per_doc() -> f64 {
    6.0
}
fn default_topic_max_topic_share() -> f64 {
    0.60
}
fn default_topic_min_fuzzy_silhouette() -> f64 {
    -1.0
}
fn default_topic_clustering_method() -> String {
    // The bake-off (2026-06-13) found the graph-hybrid engine the clear winner
    // on real embeddings: non-degenerate, clean 1-topic-per-doc partition, real
    // coherence (NPMI 0.39) + diversity (0.71) + modularity (1.22), and ~10×
    // faster than the FCM embedding tracks (which stay diffuse/poorly-separated
    // even after PCA). Works for prose too (fuses the semantic-similarity edges
    // when import/call edges are sparse). Override per `topic_clustering_method`.
    "graph".to_string()
}
fn default_topic_reduce_dim() -> usize {
    30
}
fn default_topic_graph_edge_weights() -> Vec<f64> {
    vec![1.0, 1.0, 1.0]
}
fn default_topic_graph_resolution() -> f64 {
    1.0
}
fn default_topic_llm_labels() -> bool {
    true
}
fn default_topic_llm_backend() -> String {
    "qwen3-4b".to_string()
}
fn default_graph_analysis_interval() -> u64 {
    7200
} // 2 hours
fn default_symbol_extraction_interval() -> u64 {
    7200
} // 2 hours — matches graph-analysis cadence
fn default_fuzzy_sync_interval() -> u64 {
    1800
} // 30 min
fn default_ngram_lm_train_interval() -> u64 {
    43200
} // 12 h
fn default_topic_dendrogram_interval() -> u64 {
    43200
} // 12 h
fn default_quality_history_interval() -> u64 {
    21_600 // 6h
}
fn default_retrieval_eval_project() -> String {
    "pgmcp".to_string()
}

fn default_topics_size_history_interval() -> u64 {
    21_600 // 6h
}

fn default_tool_policy_interval() -> u64 {
    21_600 // 6h
}

fn default_findings_promotion_interval() -> u64 {
    21_600 // 6h
}

fn default_concurrency_scan_interval() -> u64 {
    0 // disabled by default; opt-in (heavier: betweenness + cycle detection)
}

fn default_cron_history_retention_days() -> i64 {
    30
}

fn default_embedding_migration_interval() -> u64 {
    0
} // disabled by default; operator enables via config when ready to migrate
fn default_embedding_migration_batch_size() -> usize {
    64
}
fn default_embedding_migration_max_batches() -> usize {
    32
}

impl Config {
    /// Load configuration from the default path or the specified path.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let config_path = match path {
            Some(p) => p.to_path_buf(),
            None => Self::default_config_path(),
        };

        let mut config = if !config_path.exists() {
            Config::default()
        } else {
            let content = std::fs::read_to_string(&config_path)
                .map_err(|e| PgmcpError::file_io(&config_path, e))?;
            toml::from_str(&content)?
        };
        // The tracker trust-boundary credential. Prefer an env var over the
        // config file: a config file may be committed to git, and pgmcp INDEXES
        // toml config files into the searchable `file_chunks` corpus, so a token
        // written there is retrievable via grep / semantic_search. The env
        // override keeps the secret off disk and wins over file config.
        if let Ok(token) = std::env::var("PGMCP_TRACKER_USER_TOKEN")
            && !token.is_empty()
        {
            config.tracker.user_token = Some(token);
        }
        Ok(config)
    }

    /// Resolve the config file path from an optional user-provided path or the default.
    pub fn resolve_path(custom: Option<&Path>) -> PathBuf {
        match custom {
            Some(p) => p.to_path_buf(),
            None => Self::default_config_path(),
        }
    }

    /// Default config file path: ~/.config/pgmcp/config.toml
    pub fn default_config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("~/.config"))
            .join("pgmcp")
            .join("config.toml")
    }

    /// Generate default config content as TOML string.
    pub fn default_toml() -> String {
        let config = Config::default();
        toml::to_string_pretty(&config).expect("Failed to serialize default config")
    }

    /// Write the default config to the default path.
    pub fn write_default() -> Result<PathBuf> {
        let path = Self::default_config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| PgmcpError::file_io(parent, e))?;
        }
        std::fs::write(&path, Self::default_toml()).map_err(|e| PgmcpError::file_io(&path, e))?;
        Ok(path)
    }

    /// Return the `~/.claude/` directory if it exists.
    pub fn claude_dir() -> Option<PathBuf> {
        dirs::home_dir()
            .map(|h| h.join(".claude"))
            .filter(|p| p.is_dir())
    }

    /// Return the `~/.codex/` directory if it exists.
    pub fn codex_dir() -> Option<PathBuf> {
        dirs::home_dir()
            .map(|h| h.join(".codex"))
            .filter(|p| p.is_dir())
    }

    /// Return the `~/Papers/` directory if it exists. When present, the
    /// scanner auto-discovers it as a synthetic project named `Papers`
    /// (mirroring the `~/.claude/` and `~/.codex/` precedent — no `.git/`
    /// required). Returns `None` if the directory is absent so users
    /// without an academic-papers folder pay no cost.
    pub fn papers_dir() -> Option<PathBuf> {
        dirs::home_dir()
            .map(|h| h.join("Papers"))
            .filter(|p| p.is_dir())
    }

    /// Return the `~/Documents/` directory if it exists. Auto-discovered
    /// as a synthetic project named `Documents`. See `papers_dir` for the
    /// design rationale; same `is_dir()` guard pattern.
    pub fn documents_dir() -> Option<PathBuf> {
        dirs::home_dir()
            .map(|h| h.join("Documents"))
            .filter(|p| p.is_dir())
    }

    /// Upgrade an existing config file by merging new defaults while preserving
    /// user customizations. Returns the path that was written.
    pub fn upgrade(path: Option<&Path>) -> Result<PathBuf> {
        let config_path = match path {
            Some(p) => p.to_path_buf(),
            None => Self::default_config_path(),
        };

        let defaults_toml: TomlValue =
            toml::from_str(&Self::default_toml()).expect("Default config must be valid TOML");

        if config_path.exists() {
            let user_content = std::fs::read_to_string(&config_path)
                .map_err(|e| PgmcpError::file_io(&config_path, e))?;
            let user_toml: TomlValue = toml::from_str(&user_content)?;
            let merged = merge_toml_values(defaults_toml, user_toml);
            let output = toml::to_string_pretty(&merged).expect("Merged TOML must serialize");
            std::fs::write(&config_path, output)
                .map_err(|e| PgmcpError::file_io(&config_path, e))?;
        } else {
            // No existing config — just write defaults
            if let Some(parent) = config_path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| PgmcpError::file_io(parent, e))?;
            }
            std::fs::write(&config_path, Self::default_toml())
                .map_err(|e| PgmcpError::file_io(&config_path, e))?;
        }

        Ok(config_path)
    }
}

/// Recursively merge two TOML values. `user` values take precedence over `defaults`.
/// - Tables: recursively merged; new default keys are added; user keys preserved.
/// - Arrays: user entries kept, new default entries (not already present) appended.
/// - Scalars: user value wins.
pub fn merge_toml_values(defaults: TomlValue, user: TomlValue) -> TomlValue {
    match (defaults, user) {
        (TomlValue::Table(mut def_table), TomlValue::Table(user_table)) => {
            for (key, user_val) in user_table {
                let merged = if let Some(def_val) = def_table.remove(&key) {
                    merge_toml_values(def_val, user_val)
                } else {
                    user_val
                };
                def_table.insert(key, merged);
            }
            TomlValue::Table(def_table)
        }
        (TomlValue::Array(def_arr), TomlValue::Array(user_arr)) => {
            let mut merged = user_arr;
            for def_item in def_arr {
                if !merged.contains(&def_item) {
                    merged.push(def_item);
                }
            }
            TomlValue::Array(merged)
        }
        // User scalar wins over default scalar
        (_defaults, user) => user,
    }
}

/// Per-project override config (.pgmcp.toml in project root).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ProjectOverride {
    #[serde(default)]
    pub indexer: Option<ProjectIndexerOverride>,
    #[serde(default)]
    pub git: Option<GitConfig>,
    /// Per-project phonetic-framework override (P14.4). When
    /// `rules_path` is set, the daemon's event_processor installs a
    /// `PgmcpPhonetics` watcher on that path so the index-backed
    /// phonetic search (`phonetic_symbol_search`) and query
    /// correction (`correct_query`) pick up the project's rule set
    /// when their `project` param resolves to this root.
    #[serde(default)]
    pub phonetics: Option<ProjectPhoneticsOverride>,
    /// Declared layer-dependency rules for reflexion-model conformance checking
    /// (Murphy-Notkin-Sullivan, TSE 2001). When present, `architecture_violations`
    /// maps files to layers by path prefix and flags import edges that violate
    /// the declared `allow` rules (divergences). Absent ⇒ the reflexion check is
    /// simply skipped (purely additive). (graph-roadmap Phase 3.2)
    #[serde(default)]
    pub architecture: Option<ArchitectureRules>,
    /// Per-project tracker behavior (Phase 3). Controls the opt-in
    /// `findings-promotion` cron for this project.
    #[serde(default)]
    pub tracker: Option<ProjectTrackerOverride>,
}

/// Per-project tracker override (`[tracker]` in `.pgmcp.toml`). All knobs are
/// **default OFF** — auto-promotion of analytic findings into work items is a
/// write-side action that a project opts into explicitly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ProjectTrackerOverride {
    /// Enable the `findings-promotion` cron for this project: idempotently
    /// materialize high-confidence `bug_prediction` files and high-severity
    /// `documented_tech_debt` markers into `pending` work items. Default
    /// `false` (the cron skips every project that has not opted in).
    #[serde(default)]
    pub auto_promote_findings: bool,
    /// Minimum `bug_prediction` score for a file to be promoted (default 0.6).
    /// Only consulted when `auto_promote_findings` is on.
    #[serde(default = "default_findings_bug_score_threshold")]
    pub findings_bug_score_threshold: f64,
}

fn default_findings_bug_score_threshold() -> f64 {
    0.6
}

/// Declared layered-architecture rules for reflexion conformance (Phase 3.2).
///
/// Example `.pgmcp.toml`:
/// ```toml
/// [architecture]
/// layers = [
///   { name = "api",    paths = ["src/api/", "src/mcp/"] },
///   { name = "domain", paths = ["src/graph/", "src/code_analysis/"] },
///   { name = "data",   paths = ["src/db/"] },
/// ]
/// allow = [
///   { from = "api",    to = "domain" },
///   { from = "domain", to = "data" },
/// ]
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ArchitectureRules {
    /// Layers, each a name + the path prefixes (relative to the project root)
    /// whose files belong to it. A file is assigned to the FIRST layer with a
    /// matching prefix, so list more-specific prefixes first.
    #[serde(default)]
    pub layers: Vec<LayerDef>,
    /// Allowed directed dependencies: a file in `from` MAY import a file in
    /// `to`. Same-layer imports are always allowed and need not be listed. Any
    /// import edge that is neither same-layer nor listed here is a divergence.
    #[serde(default)]
    pub allow: Vec<AllowRule>,
}

/// One declared architectural layer: a name and the path prefixes it owns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct LayerDef {
    pub name: String,
    #[serde(default)]
    pub paths: Vec<String>,
}

/// One permitted directed layer→layer dependency.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AllowRule {
    pub from: String,
    pub to: String,
}

/// Per-project phonetic rule + language override (P14.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ProjectPhoneticsOverride {
    /// Path to a `.llev` rule file (typically `.pgmcp/rules.llev`
    /// under the project root). When set, the daemon's
    /// `PgmcpPhonetics::watch` watcher hot-reloads on file change
    /// or deletion.
    pub rules_path: Option<PathBuf>,
    /// BCP-47 language tag (e.g. `"en-us"`, `"fr"`). Recorded for
    /// diagnostics and consumed by `PgmcpPhonetics::for_language`
    /// when `rules_path` is unset.
    pub language: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectIndexerOverride {
    pub exclude_patterns: Option<Vec<String>>,
    pub file_types: Option<Vec<FileTypeMapping>>,
    pub max_file_size_bytes: Option<u64>,
    /// Per-project source-form priority (replaces the global list rather
    /// than merging — for an ordered list, replace semantics are clearer
    /// than OR).
    pub source_priority: Option<Vec<String>>,
    /// Per-project cap on binary document source bytes; overrides the
    /// global `[indexer] max_document_source_bytes`.
    pub max_document_source_bytes: Option<u64>,
    /// Per-project cap on extracted text size.
    pub max_extracted_text_bytes: Option<usize>,
    /// Per-project extraction subprocess timeout in seconds.
    pub document_extraction_timeout_secs: Option<u64>,
}

/// Git history indexing configuration for a project.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct GitConfig {
    /// Enable git history indexing (commit messages + diffs) for this project.
    #[serde(default)]
    pub index_history: bool,
    /// Auto-link work items referenced in commit messages (`#<public_id>` /
    /// `fixes <public_id>`) and run the agent-grade auto-transition (at most to
    /// a verify *candidate* — never `verified`). `None` (default) means "on when
    /// `index_history` is on"; set `false` explicitly to opt out while still
    /// indexing history. Consult via [`GitConfig::auto_link_items_enabled`].
    #[serde(default)]
    pub auto_link_items: Option<bool>,
}

impl GitConfig {
    /// Whether commit→work-item auto-linkage is enabled. Defaults to ON when
    /// `index_history` is on (auto-linkage needs the indexed commits) and OFF
    /// otherwise; an explicit `auto_link_items` always wins.
    pub fn auto_link_items_enabled(&self) -> bool {
        self.auto_link_items.unwrap_or(self.index_history)
    }
}

impl ProjectOverride {
    pub fn load(project_root: &Path) -> Option<Self> {
        let path = project_root.join(".pgmcp.toml");
        if !path.exists() {
            return None;
        }
        // Explicit error logging on read / parse failure. The prior
        // `.ok()?` chain silently dropped both classes of error, so an
        // operator typo (e.g. `[git] index_history = trueeeee`) would
        // disable per-project config with no signal — the project
        // would just not behave the way they configured it.
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "per-project .pgmcp.toml unreadable; ignoring overrides for this project"
                );
                return None;
            }
        };
        match toml::from_str(&content) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "per-project .pgmcp.toml is malformed TOML; ignoring overrides for this project"
                );
                None
            }
        }
    }

    /// Default per-project config TOML content.
    pub fn default_toml() -> String {
        let default = ProjectOverride {
            indexer: None,
            git: Some(GitConfig::default()),
            phonetics: None,
            architecture: None,
            // Opt-in (default OFF) — omitted from the default .pgmcp.toml so a
            // fresh project does not auto-promote findings until it sets
            // `[tracker] auto_promote_findings = true`.
            tracker: None,
        };
        toml::to_string_pretty(&default).expect("Failed to serialize default project override")
    }

    /// Write the default .pgmcp.toml to a project root.
    pub fn write_default(project_root: &Path) -> Result<PathBuf> {
        let path = project_root.join(".pgmcp.toml");
        std::fs::write(&path, Self::default_toml()).map_err(|e| PgmcpError::file_io(&path, e))?;
        Ok(path)
    }

    /// Upgrade an existing .pgmcp.toml by merging new defaults while preserving
    /// user customizations.
    pub fn upgrade(project_root: &Path) -> Result<PathBuf> {
        let path = project_root.join(".pgmcp.toml");

        let defaults_toml: TomlValue = toml::from_str(&Self::default_toml())
            .expect("Default project override must be valid TOML");

        if path.exists() {
            let user_content =
                std::fs::read_to_string(&path).map_err(|e| PgmcpError::file_io(&path, e))?;
            let user_toml: TomlValue = toml::from_str(&user_content)?;
            let merged = merge_toml_values(defaults_toml, user_toml);
            let output = toml::to_string_pretty(&merged).expect("Merged TOML must serialize");
            std::fs::write(&path, output).map_err(|e| PgmcpError::file_io(&path, e))?;
        } else {
            std::fs::write(&path, Self::default_toml())
                .map_err(|e| PgmcpError::file_io(&path, e))?;
        }

        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_parses() {
        let toml_str = Config::default_toml();
        let _config: Config = toml::from_str(&toml_str).expect("Default config should parse");
    }

    #[test]
    fn tracker_user_token_env_override_wins_and_is_off_by_default() {
        // The PGMCP_TRACKER_USER_TOKEN override keeps the trust-boundary secret
        // off disk (config files are indexed into the searchable corpus). nextest
        // runs each test in its own process, so this env mutation is isolated.
        // Use a non-existent config path so `load` starts from Config::default()
        // (user_token = None) and then applies the env override deterministically.
        let missing = Path::new("/nonexistent/pgmcp/config-does-not-exist.toml");

        // SAFETY: single-threaded test process (nextest), env restored below.
        unsafe { std::env::remove_var("PGMCP_TRACKER_USER_TOKEN") };
        let cfg = Config::load(Some(missing)).expect("load with missing path → defaults");
        assert_eq!(
            cfg.tracker.user_token, None,
            "no env, no file → token must stay unset (fail-closed)"
        );

        unsafe { std::env::set_var("PGMCP_TRACKER_USER_TOKEN", "s3cret-from-env") };
        let cfg = Config::load(Some(missing)).expect("load with env override");
        assert_eq!(
            cfg.tracker.user_token.as_deref(),
            Some("s3cret-from-env"),
            "env var must populate the tracker token"
        );

        // Empty env value must NOT set a token (treated as absent).
        unsafe { std::env::set_var("PGMCP_TRACKER_USER_TOKEN", "") };
        let cfg = Config::load(Some(missing)).expect("load with empty env");
        assert_eq!(
            cfg.tracker.user_token, None,
            "empty env value must be treated as unset, not an empty token"
        );

        unsafe { std::env::remove_var("PGMCP_TRACKER_USER_TOKEN") };
    }

    #[test]
    fn test_extension_map() {
        let config = IndexerConfig::default();
        let map = config.extension_map();
        assert_eq!(map.get("rs"), Some(&"rust".to_string()));
        assert_eq!(map.get("py"), Some(&"python".to_string()));
    }

    #[test]
    fn test_index_freshness_defaults() {
        // Pin the bounded-retry cap and the reconcile-backstop cadence (the
        // `Default` impls call the `default_*` fns) so a silent change is caught.
        assert_eq!(IndexerConfig::default().max_index_retries, 5);
        assert_eq!(CronConfig::default().index_reconcile_interval_secs, 1800);
        assert_eq!(default_max_index_retries(), 5);
        assert_eq!(default_index_reconcile(), 1800);
    }

    #[test]
    fn test_is_configured_extension() {
        let config = IndexerConfig::default();
        assert!(config.is_configured_extension(Path::new("foo.rs")));
        assert!(config.is_configured_extension(Path::new("bar.py")));
        assert!(!config.is_configured_extension(Path::new("baz.exe")));
    }

    #[test]
    fn test_language_for_path() {
        let config = IndexerConfig::default();
        assert_eq!(
            config.language_for_path(Path::new("foo.rs")),
            Some("rust".into())
        );
        assert_eq!(config.language_for_path(Path::new("foo.xyz")), None);
    }

    #[test]
    fn test_database_url() {
        let db = DatabaseConfig::default();
        let url = db.connection_url();
        assert!(url.starts_with("postgres://pgmcp@localhost:5432/pgmcp"));
    }

    #[test]
    fn test_work_pool_config_defaults() {
        let wpc = WorkPoolConfig::default();
        assert_eq!(wpc.min_threads, 2);
        assert!(wpc.resolved_max_threads() >= 1);
        assert_eq!(wpc.resolved_initial_threads(), 2);
    }

    #[test]
    fn test_new_file_types() {
        let config = IndexerConfig::default();
        assert!(config.is_configured_extension(Path::new("script.sh")));
        assert!(config.is_configured_extension(Path::new("data.jsonl")));
        assert_eq!(
            config.language_for_path(Path::new("script.sh")),
            Some("shell".into())
        );
        assert_eq!(
            config.language_for_path(Path::new("data.jsonl")),
            Some("jsonl".into())
        );
    }

    /// Regression test for the Tier-0e extensions added 2026-05-01. Every
    /// extension here must round-trip to a language whose `LanguageRegistry`
    /// returns `Some` — otherwise the symbol-extraction cron would skip files
    /// of that type.
    #[test]
    fn test_default_file_types_includes_tier_0e_languages() {
        let config = IndexerConfig::default();
        for (ext, expected_lang) in [
            ("java", "java"),
            ("scala", "scala"),
            ("c", "c"),
            ("h", "c"),
            ("cpp", "cpp"),
            ("cc", "cpp"),
            ("cxx", "cpp"),
            ("hpp", "cpp"),
            ("hxx", "cpp"),
            ("clj", "clojure"),
            ("cljs", "clojurescript"),
            ("cljc", "clojure"),
            ("tsx", "tsx"),
        ] {
            let path_str = format!("file.{}", ext);
            let path = Path::new(&path_str);
            assert!(
                config.is_configured_extension(path),
                "missing default mapping for .{}",
                ext
            );
            assert_eq!(
                config.language_for_path(path),
                Some(expected_lang.to_string()),
                "wrong language for .{}",
                ext
            );
            // Cross-check: the language must be one that `LanguageRegistry`
            // routes to a backend.
            assert!(
                crate::parsing::LanguageRegistry::for_language(expected_lang).is_some(),
                "no backend registered for language `{}` (mapped from .{})",
                expected_lang,
                ext
            );
        }
    }

    #[test]
    fn test_merge_toml_scalars_user_wins() {
        let defaults: TomlValue = toml::from_str(r#"key = "default""#).expect("parse");
        let user: TomlValue = toml::from_str(r#"key = "custom""#).expect("parse");
        let merged = merge_toml_values(defaults, user);
        assert_eq!(merged["key"].as_str(), Some("custom"));
    }

    #[test]
    fn test_merge_toml_tables_add_new_keys() {
        let defaults: TomlValue = toml::from_str(
            r#"
            [section]
            existing = "default"
            new_key = "added"
        "#,
        )
        .expect("parse");
        let user: TomlValue = toml::from_str(
            r#"
            [section]
            existing = "custom"
        "#,
        )
        .expect("parse");
        let merged = merge_toml_values(defaults, user);
        assert_eq!(merged["section"]["existing"].as_str(), Some("custom"));
        assert_eq!(merged["section"]["new_key"].as_str(), Some("added"));
    }

    #[test]
    fn test_merge_toml_arrays_union() {
        let defaults: TomlValue = toml::from_str(
            r#"
            items = ["a", "b", "c"]
        "#,
        )
        .expect("parse");
        let user: TomlValue = toml::from_str(
            r#"
            items = ["b", "d"]
        "#,
        )
        .expect("parse");
        let merged = merge_toml_values(defaults, user);
        let arr = merged["items"].as_array().expect("should be array");
        assert!(arr.contains(&TomlValue::String("b".into())));
        assert!(arr.contains(&TomlValue::String("d".into())));
        assert!(arr.contains(&TomlValue::String("a".into())));
        assert!(arr.contains(&TomlValue::String("c".into())));
    }

    #[test]
    fn test_merge_toml_preserves_user_only_keys() {
        let defaults: TomlValue = toml::from_str(r#"a = 1"#).expect("parse");
        let user: TomlValue = toml::from_str(
            r#"
            a = 2
            user_only = 42
        "#,
        )
        .expect("parse");
        let merged = merge_toml_values(defaults, user);
        assert_eq!(merged["a"].as_integer(), Some(2));
        assert_eq!(merged["user_only"].as_integer(), Some(42));
    }

    /// Regression: every document extension added in Phase 5 must be
    /// configured and map to its expected language. The language strings
    /// MUST NOT collide with any tree-sitter backend name in
    /// `LanguageRegistry`, since that's how the symbol-extraction cron
    /// decides to skip these files (return `None` from `for_language`).
    #[test]
    fn test_default_file_types_includes_document_languages() {
        let config = IndexerConfig::default();
        for (ext, expected_lang) in [
            ("pdf", "pdf"),
            ("ps", "postscript"),
            ("eps", "postscript"),
            ("tex", "latex"),
            ("latex", "latex"),
            ("bib", "bibtex"),
            ("org", "org"),
            ("rst", "rst"),
            ("docx", "docx"),
            ("doc", "doc"),
            ("rtf", "rtf"),
            ("odt", "odt"),
            ("epub", "epub"),
            ("txt", "text"),
        ] {
            let path_str = format!("file.{}", ext);
            let path = Path::new(&path_str);
            assert!(
                config.is_configured_extension(path),
                "missing document mapping for .{}",
                ext
            );
            assert_eq!(
                config.language_for_path(path),
                Some(expected_lang.to_string()),
                "wrong language for .{}",
                ext
            );
            // None of these languages should resolve to a tree-sitter backend.
            assert!(
                crate::parsing::LanguageRegistry::for_language(expected_lang).is_none(),
                "document language `{}` (.{}) collides with tree-sitter backend",
                expected_lang,
                ext
            );
        }
    }

    #[test]
    fn test_default_file_types_includes_formal_verification_languages() {
        let config = IndexerConfig::default();
        for (ext, expected_lang) in [
            ("v", "coq"),
            ("tla", "tlaplus"),
            ("smt2", "smt2"),
            ("smt", "smt2"),
            ("lean", "lean"),
            ("sage", "sage"),
            ("thy", "isabelle"),
            ("agda", "agda"),
            ("lagda", "agda"),
            ("idr", "idris"),
            ("lidr", "idris"),
            ("ipkg", "idris"),
            ("dfy", "dafny"),
            ("fst", "fstar"),
            ("fsti", "fstar"),
            ("mlw", "why3"),
            ("als", "alloy"),
            ("pml", "promela"),
            ("ec", "easycrypt"),
            ("eca", "easycrypt"),
            ("spthy", "tamarin"),
            ("pvs", "pvs"),
            ("acl2", "acl2"),
            ("mm", "metamath"),
            ("cv", "cryptoverif"),
            ("ocv", "cryptoverif"),
        ] {
            let path_str = format!("file.{}", ext);
            let path = Path::new(&path_str);
            assert!(
                config.is_configured_extension(path),
                "missing FV mapping for .{}",
                ext
            );
            assert_eq!(
                config.language_for_path(path),
                Some(expected_lang.to_string()),
                "wrong language for .{}",
                ext
            );
        }
    }

    #[test]
    fn test_default_exclude_patterns_includes_fv_build_artifacts() {
        let config = IndexerConfig::default();
        for pattern in [
            "_build",
            ".lake",
            "lake-packages",
            "*.vo",
            "*.vok",
            "*.vos",
            "*.glob",
            "*.agdai",
            ".tlaplus-cache",
        ] {
            assert!(
                config.exclude_patterns.iter().any(|p| p == pattern),
                "missing FV exclude pattern: {}",
                pattern
            );
        }
    }

    #[test]
    fn test_cfg_in_tlaplus_project_maps_to_tlaplus() {
        let config = IndexerConfig::default();
        let mut siblings = HashSet::new();
        siblings.insert("tla".to_string());
        siblings.insert("cfg".to_string());
        assert_eq!(
            config.language_for_path_in_context(Path::new("MC.cfg"), &siblings),
            Some("tlaplus".to_string()),
            "`.cfg` should map to tlaplus when a sibling `.tla` exists"
        );
        assert!(
            config.is_configured_extension_in_context(Path::new("MC.cfg"), &siblings),
            "`.cfg` should be included when a sibling `.tla` exists"
        );
    }

    #[test]
    fn test_cfg_outside_tlaplus_project_is_dropped() {
        let config = IndexerConfig::default();
        let mut siblings = HashSet::new();
        siblings.insert("conf".to_string());
        siblings.insert("yaml".to_string());
        assert_eq!(
            config.language_for_path_in_context(Path::new("nginx.cfg"), &siblings),
            None,
            "`.cfg` without sibling `.tla` should be dropped"
        );
        assert!(
            !config.is_configured_extension_in_context(Path::new("nginx.cfg"), &siblings),
            "`.cfg` without sibling `.tla` should not be included"
        );
    }

    #[test]
    fn test_language_for_path_in_context_falls_back_to_path_lookup() {
        let config = IndexerConfig::default();
        let empty: HashSet<String> = HashSet::new();
        assert_eq!(
            config.language_for_path_in_context(Path::new("foo.rs"), &empty),
            Some("rust".to_string()),
        );
        assert_eq!(
            config.language_for_path_in_context(Path::new("foo.tla"), &empty),
            Some("tlaplus".to_string()),
        );
    }

    #[test]
    fn test_indexer_config_document_defaults() {
        let cfg = IndexerConfig::default();
        assert_eq!(cfg.max_document_source_bytes, 100 * 1024 * 1024);
        assert_eq!(cfg.max_extracted_text_bytes, 50 * 1024 * 1024);
        assert_eq!(cfg.document_extraction_timeout_secs, 30);
        // Priority list contains source forms first, output forms last.
        let prio = &cfg.source_priority;
        let pos_org = prio.iter().position(|e| e == "org").expect("org present");
        let pos_tex = prio.iter().position(|e| e == "tex").expect("tex present");
        let pos_pdf = prio.iter().position(|e| e == "pdf").expect("pdf present");
        assert!(
            pos_org < pos_tex && pos_tex < pos_pdf,
            "expected org < tex < pdf in source priority"
        );
    }

    #[test]
    fn test_project_override_with_document_fields() {
        let toml_str = r#"
            [indexer]
            source_priority = ["org", "pdf"]
            max_document_source_bytes = 209715200
            max_extracted_text_bytes = 104857600
            document_extraction_timeout_secs = 60
        "#;
        let parsed: ProjectOverride = toml::from_str(toml_str).expect("parse");
        let idx = parsed.indexer.expect("indexer section present");
        assert_eq!(
            idx.source_priority.as_deref(),
            Some(&["org".to_string(), "pdf".to_string()][..])
        );
        assert_eq!(idx.max_document_source_bytes, Some(209715200));
        assert_eq!(idx.max_extracted_text_bytes, Some(104857600));
        assert_eq!(idx.document_extraction_timeout_secs, Some(60));
    }

    #[test]
    fn test_synthetic_dir_helpers_optional() {
        // These helpers return Option<PathBuf>; the contract is "Some when
        // the directory exists, None otherwise" — we only assert the type
        // contract here since the directories' existence depends on the
        // host filesystem.
        let _: Option<PathBuf> = Config::papers_dir();
        let _: Option<PathBuf> = Config::documents_dir();
    }

    #[test]
    fn test_project_override_default_toml_parses() {
        let toml_str = ProjectOverride::default_toml();
        let _parsed: ProjectOverride =
            toml::from_str(&toml_str).expect("Default project override TOML should parse");
    }

    #[test]
    fn test_project_override_with_git_config() {
        let toml_str = r#"
            [git]
            index_history = true
        "#;
        let parsed: ProjectOverride = toml::from_str(toml_str).expect("parse");
        assert!(
            parsed
                .git
                .expect("git section should be present")
                .index_history
        );
    }

    #[test]
    fn test_git_history_cron_default() {
        let config = CronConfig::default();
        assert_eq!(config.git_history_index_interval_secs, 3600);
    }

    #[test]
    fn test_similarity_cron_defaults() {
        let config = CronConfig::default();
        assert_eq!(config.similarity_scan_interval_secs, 21600);
        assert!((config.similarity_threshold - 0.85).abs() < f64::EPSILON);
        assert_eq!(config.similarity_top_k, 10);
    }

    #[test]
    fn test_topic_clustering_cron_defaults() {
        let config = CronConfig::default();
        assert_eq!(config.topic_scan_interval_secs, 43200);
        assert_eq!(config.topic_min_cluster_size, 5);
        assert!(config.topic_num_clusters.is_none());
        assert!((config.topic_fuzziness - 2.0).abs() < f64::EPSILON);
        assert_eq!(config.topic_fcm_max_iters, 100);
        assert!((config.topic_fcm_tolerance - 1e-5).abs() < 1e-12);
        assert!((config.topic_membership_threshold - 0.05).abs() < f64::EPSILON);
        assert_eq!(config.topic_label_top_k, 5);
    }

    #[test]
    fn test_symbol_extraction_cron_defaults() {
        let config = CronConfig::default();
        assert_eq!(config.symbol_extraction_interval_secs, 7200);
        assert_eq!(config.ready_delay_symbol_extraction_secs, 1800);
    }

    // ========================================================================
    // Property tests for merge_toml_values
    // ========================================================================

    use proptest::prelude::*;

    proptest! {
        /// Scalar merge: user value always wins.
        #[test]
        fn prop_merge_user_scalar_wins(def in -100i64..100, user in -100i64..100) {
            let d = TomlValue::Integer(def);
            let u = TomlValue::Integer(user);
            let merged = merge_toml_values(d, u);
            prop_assert_eq!(merged.as_integer(), Some(user));
        }

        /// Array merge: result starts with user verbatim (including any
        /// duplicates the user wrote), then appends default items not
        /// already in user. User items always appear first.
        #[test]
        fn prop_merge_array_appends_missing_defaults(
            def in prop::collection::vec(0i64..20, 0..10),
            user in prop::collection::vec(0i64..20, 0..10),
        ) {
            let d = TomlValue::Array(def.iter().map(|&x| TomlValue::Integer(x)).collect());
            let u = TomlValue::Array(user.iter().map(|&x| TomlValue::Integer(x)).collect());
            let merged = merge_toml_values(d, u);
            let arr = merged.as_array().expect("array");
            // User portion is a verbatim prefix of the merged output.
            for (i, &v) in user.iter().enumerate() {
                prop_assert_eq!(arr[i].as_integer(), Some(v),
                    "user item {} at pos {} changed", v, i);
            }
            // Every default value appears somewhere (either because it was
            // already in user or because it was appended).
            for &v in &def {
                prop_assert!(arr.iter().any(|x| x.as_integer() == Some(v)),
                    "default value {} missing from merged array", v);
            }
            // Length is bounded above by |user| + |def| — no accidental
            // multiplication.
            prop_assert!(arr.len() <= user.len() + def.len());
        }

        /// Table merge: keys only in user end up in result, keys only in
        /// default end up in result, keys in both are recursively merged.
        #[test]
        fn prop_merge_tables_preserve_both_sides(
            def_key in "[a-z]{1,6}",
            user_key in "[a-z]{1,6}",
            def_val in 0i64..100,
            user_val in 0i64..100,
        ) {
            prop_assume!(def_key != user_key);
            let mut d_table = toml::map::Map::new();
            d_table.insert(def_key.clone(), TomlValue::Integer(def_val));
            let d = TomlValue::Table(d_table);

            let mut u_table = toml::map::Map::new();
            u_table.insert(user_key.clone(), TomlValue::Integer(user_val));
            let u = TomlValue::Table(u_table);

            let merged = merge_toml_values(d, u);
            let t = merged.as_table().expect("table");
            prop_assert_eq!(t.get(&def_key).and_then(|v| v.as_integer()), Some(def_val));
            prop_assert_eq!(t.get(&user_key).and_then(|v| v.as_integer()), Some(user_val));
        }

        /// Idempotence: merge(merge(d, u), u) == merge(d, u) for scalars.
        #[test]
        fn prop_merge_idempotent_for_scalars(def in 0i64..100, user in 0i64..100) {
            let d = TomlValue::Integer(def);
            let u = TomlValue::Integer(user);
            let first = merge_toml_values(d.clone(), u.clone());
            let again = merge_toml_values(first.clone(), u);
            prop_assert_eq!(first.as_integer(), again.as_integer());
        }
    }
}
