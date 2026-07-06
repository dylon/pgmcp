// Lifted above the default (128) for the large `serde_json::json!` stats-snapshot
// literal in `src/stats/tracker.rs` (see src/lib.rs); unrelated to the
// AcceptanceCriterion serde fix (adjacent tagging, ADR-006).
#![recursion_limit = "1024"]

// Phase 11: install mimalloc as the global allocator. Eliminates
// glibc's mmap/munmap per-allocation `__mprotect` syscall pattern
// that consumed ~45% CPU during large imports under the prior
// allocator. The existing `mallopt(M_ARENA_MAX, 2)` in
// `cap_malloc_arenas()` stays as belt-and-suspenders — it becomes a
// no-op while mimalloc is the active allocator but kicks back in if
// mimalloc is ever disabled. Plan reference:
// ~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md
// Phase 11.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod a2a;
mod adoption;
mod api;
mod category;
mod cli;
#[allow(dead_code)]
mod code_analysis;
mod concurrency;
mod config;
mod context;
mod cron;
mod csm;
mod daemon;
mod daemon_state;
mod datatable;
mod db;
mod deps;
mod digest;
mod docguidelines;
mod embed;
mod engprinciples;
mod error;
mod experiment;
mod fcm;
mod feedback;
#[allow(dead_code)]
mod fuzzy;
mod graph;
mod health;
mod hierarchy;
mod indexer;
mod llm;
mod logging;
mod mandates;
mod mcp;
#[allow(dead_code)]
mod mmap_array;
#[allow(dead_code)]
mod neural;
mod ontology;
mod parsing;
mod patterns;
mod proc_clients;
mod quality;
mod reactive;
mod realtime;
mod render;
mod reranker;
mod rmas;
mod sessions;
mod shutdown;
mod stats;
// Phase 5 control plane is complete + tested in-module; its tool/orchestrator
// wiring lands later, so the binary does not yet call the public surface.
#[allow(dead_code)]
mod tape;
mod tools_catalog;
mod topic_analysis;
mod topic_apps;
#[allow(dead_code)]
mod topic_store;
mod tracker;
mod voting;
#[allow(dead_code)]
mod wfst;
mod work_pool;
mod worklog;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::cli::analyze::AnalyzeJob;
use crate::cli::results::ResultsKind;

#[derive(Parser)]
#[command(
    name = "pgmcp",
    version,
    about = "PostgreSQL + pgvector MCP File Indexer"
)]
struct Cli {
    /// Path to configuration file
    #[arg(short, long)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run in foreground (stdout logging, for debugging)
    Serve,
    /// Run as systemd daemon (sd-notify, file logging)
    Daemon,
    /// Print statistics from running instance (alias: `stats`)
    #[command(alias = "stats")]
    Statistics,
    /// Trigger full re-index of all workspaces. Refuses to run if the
    /// daemon is currently listening on `mcp.host:port` — stop the daemon
    /// first or pass `--force` to bypass.
    Reindex {
        /// Bypass the running-daemon check. Use only when you're certain
        /// the daemon is stopped (e.g. socket is lingering after kill).
        #[arg(long)]
        force: bool,
    },
    /// Generate default config at ~/.config/pgmcp/config.toml
    Init,
    /// Upgrade all configs: global config.toml + .pgmcp.toml in all indexed projects
    #[command(alias = "upgrade-config")]
    UpgradeConfigs {
        /// Prompt before upgrading each project's .pgmcp.toml
        #[arg(short, long)]
        interactive: bool,
    },
    /// Initialize .pgmcp.toml in the current project
    InitProject {
        /// Project directory (defaults to $PWD)
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Upgrade .pgmcp.toml with new defaults (preserves customizations)
    UpgradeProject {
        /// Project directory (defaults to $PWD)
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Print project context for the current working directory (for Claude Code hooks)
    Context {
        /// Working directory to find project for (defaults to $PWD)
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Maximum depth for file tree (default: 3)
        #[arg(long, default_value = "3")]
        depth: i32,
    },
    /// Boyscout gate: fail if an open `kind='bug'` work-item is anchored to a
    /// file touched by the current diff (ADR-022). Self-skips outside git / no DB.
    BugGate {
        /// Repository directory (defaults to $PWD).
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Also check committed changes since this git ref (e.g. `origin/main`);
        /// without it only uncommitted working-tree changes (vs HEAD) are checked.
        #[arg(long)]
        base: Option<String>,
        /// Report anchored bugs but exit 0 (advisory mode).
        #[arg(long)]
        warn_only: bool,
        /// Maximum number of anchored bugs to report.
        #[arg(long, default_value = "50")]
        limit: i64,
    },
    /// Run analysis jobs on demand (similarity scan, topic clustering, or both)
    Analyze {
        #[command(subcommand)]
        job: Option<AnalyzeJob>,
        /// Override similarity threshold (default from config)
        #[arg(long)]
        similarity_threshold: Option<f64>,
        /// Override similarity top_k (default from config)
        #[arg(long)]
        similarity_top_k: Option<i32>,
        /// Override FCM min_cluster_size for K estimation (default from config)
        #[arg(long)]
        min_cluster_size: Option<usize>,
        /// Explicit number of topic clusters (overrides auto-estimation)
        #[arg(long)]
        num_clusters: Option<usize>,
        /// FCM fuzziness exponent (default: 2.0)
        #[arg(long)]
        fuzziness: Option<f64>,
    },
    /// Print cached analysis results from the database
    Results {
        #[command(subcommand)]
        kind: Option<ResultsKind>,
        /// Maximum items to display (default: 20)
        #[arg(long, default_value = "20")]
        limit: i32,
    },
    /// Run any MCP tool from the command line (run without args to list tools)
    Tool {
        /// Tool name (omit to list all available tools)
        name: Option<String>,
        /// Tool parameters as KEY=VALUE pairs (e.g. project=Foo limit=10)
        args: Vec<String>,
        /// Output compact JSON (for piping to jq)
        #[arg(long)]
        json: bool,
        /// Show the JSON Schema for a tool's parameters
        #[arg(long)]
        schema: bool,
    },
    /// Expose a CLI agent (Claude Code / Codex) as a live A2A peer. Binds a
    /// minimal A2A JSON-RPC server on `--port` that translates inbound
    /// `tasks/send` into a `claude -p` / `codex` subprocess invocation, and
    /// optionally self-registers with a pgmcp daemon's agent registry.
    A2aAdapter {
        /// Which CLI to wrap: "claude", "codex", or "pi".
        #[arg(long)]
        kind: String,
        /// TCP port to bind the adapter's A2A server on.
        #[arg(long)]
        port: u16,
        /// Agent name to advertise (defaults: claude-code / codex-cli / pi-agent).
        #[arg(long)]
        name: Option<String>,
        /// pgmcp daemon base URL to self-register with (e.g. http://localhost:3100).
        #[arg(long)]
        register_with: Option<String>,
        /// For `--kind pi`: the models.json provider to pin the leaf to (e.g. sparky-deepseek).
        #[arg(long)]
        pi_provider: Option<String>,
        /// For `--kind pi`: the model id to pin the leaf to (e.g. deepseek-v4-flash).
        #[arg(long)]
        pi_model: Option<String>,
    },
    /// Print daemon and model state. With no MODEL argument, every section
    /// renders. MODEL filters to one of: daemon, database, embeddings,
    /// topics, similarity, graph, git.
    Status {
        /// Section name to show (omit for everything).
        model: Option<String>,
        /// Emit the full snapshot as pretty JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Import a local OSV/GHSA advisory dump into `vuln_advisories` for
    /// offline CVE matching (graph-roadmap Phase 4.5). PATH is a single
    /// `.json`, a `.jsonl`, or a directory tree of OSV JSON files. This is the
    /// documented out-of-band refresh — pgmcp never fetches advisories over the
    /// network at runtime. `cve_supply_chain` then matches the dependency
    /// inventory against the imported advisories by SemVer range.
    ImportAdvisories {
        /// Path to the OSV dump (file, .jsonl, or directory).
        path: std::path::PathBuf,
    },
    /// Perform & record scientific experiments: execute benchmark arms from a
    /// spec (CPU-pinned, governor-checked) and submit samples, or ingest a
    /// hyperfine/criterion artifact. (Open/decide/search via `pgmcp tool experiment_*`.)
    Experiment {
        #[command(subcommand)]
        sub: cli::experiment::ExperimentCmd,
    },
    /// Render an experiment's scientific ledger, or inspect ledger frontmatter.
    Ledger {
        #[command(subcommand)]
        sub: cli::ledger::LedgerCmd,
    },
    /// Train a RecursiveLink (R_in) from pre-extracted (hidden, gold) pairs
    /// (JSONL) and write the safetensors. Wires the latent trainer (ADR-009 R2);
    /// pairs are pre-extracted, so this runs without a backbone.
    TrainLink {
        /// JSONL file: one {"hidden":[..],"gold":[..]} object per line.
        #[arg(long)]
        pairs: PathBuf,
        /// Backbone hidden size (R_in dimension; hidden & gold must match it).
        #[arg(long)]
        hidden_size: usize,
        /// Output safetensors path for the trained link.
        #[arg(long)]
        output: PathBuf,
        /// Training epochs (default 3).
        #[arg(long, default_value = "3")]
        epochs: usize,
        /// AdamW learning rate (default 5e-4).
        #[arg(long, default_value = "0.0005")]
        learning_rate: f64,
        /// RNG seed (default 0).
        #[arg(long, default_value = "0")]
        seed: u64,
        /// Link architecture signature stamped into the safetensors.
        #[arg(long, default_value = "rlv1-2layer-gelu-residual")]
        signature: String,
    },
    /// Run a homogeneous RecursiveMAS latent loop on one resident Qwen3 backbone
    /// (ADR-009 R3, Tier-3 v1). Intermediate roles stay latent; only the final
    /// round's last role decodes to text. Hardware-gated: without CUDA / VRAM /
    /// the backbone GGUF the loop is unavailable and the command reports the
    /// degradation (exit 0), pointing at the Tier-2 text path.
    RmasLoop {
        /// Collaboration pattern: sequential | mixture | distillation | deliberation.
        #[arg(long)]
        pattern: String,
        /// The query to run through the latent loop.
        #[arg(long)]
        query: String,
        /// Backbone variant: 8b | 4b (default 8b).
        #[arg(long, default_value = "8b")]
        backbone: String,
        /// Recursion rounds (A₁→…→Aₙ→A₁ repeated; default 2).
        #[arg(long, default_value = "2")]
        rounds: usize,
        /// Specialist count for the mixture pattern (clamped 1..=8; default 3).
        #[arg(long, default_value = "3")]
        n_specialists: usize,
        /// Directory of per-role link safetensors (`rin__<role>.safetensors`);
        /// roles with no file get a residual-identity passthrough link.
        #[arg(long)]
        link_dir: PathBuf,
        /// Max tokens the final round decodes (default 512).
        #[arg(long, default_value = "512")]
        max_new_tokens: usize,
        /// Per-role backbones (CSV, e.g. "4b,8b,4b") to run the *heterogeneous*
        /// engine — one entry per pattern role, cross-dim outer-link hops. Omit
        /// for the homogeneous engine on a single `--backbone`.
        #[arg(long)]
        backbones: Option<String>,
    },
    /// Train an OuterLink (`R_out`) from pre-extracted (hidden_src, gold_tgt)
    /// pairs (JSONL) and write the safetensors (ADR-009 R4). The cross-dim
    /// analogue of `train-link`; pairs are pre-extracted, so it runs without a
    /// backbone (the frozen Q4 backbones' through-autograd is blocked).
    TrainOuterLink {
        /// JSONL file: one {"hidden_src":[..],"gold_tgt":[..]} object per line.
        #[arg(long)]
        pairs: PathBuf,
        /// Source backbone hidden size (`hidden_src` width).
        #[arg(long)]
        src_size: usize,
        /// Target backbone hidden size (`gold_tgt` width).
        #[arg(long)]
        tgt_size: usize,
        /// Output safetensors path for the trained outer link.
        #[arg(long)]
        output: PathBuf,
        /// Training epochs (default 3).
        #[arg(long, default_value = "3")]
        epochs: usize,
        /// AdamW learning rate (default 5e-4).
        #[arg(long, default_value = "0.0005")]
        learning_rate: f64,
        /// RNG seed (default 0).
        #[arg(long, default_value = "0")]
        seed: u64,
        /// Link architecture signature stamped into the safetensors.
        #[arg(long, default_value = "rout-v1-3layer-gelu-residual")]
        signature: String,
    },
}

/// Cap the number of glibc malloc thread arenas BEFORE the tokio runtime
/// or any worker threads are spawned. Default is 8 × num_cpus (= 512 on a
/// 64-CPU host); each arena retains its high-water mark for the life of
/// the process, so a transient burst of allocations across many threads
/// inflates RSS by tens of GB that never returns to the kernel. Capping
/// at 2 keeps RSS close to the live working set with no measurable
/// throughput cost — every thread still contends rarely enough on the
/// arena lock that lock contention is invisible compared to the embed
/// inference and DB latency that dominate this binary's wall time.
///
/// No-op on non-glibc targets.
fn cap_malloc_arenas() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }
}

fn main() -> anyhow::Result<()> {
    cap_malloc_arenas();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async_main())
}

async fn async_main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cfg = cli.config.as_deref();

    match cli.command {
        Commands::Init => cli::admin::init(),
        Commands::UpgradeConfigs { interactive } => {
            cli::admin::upgrade_configs(cfg, interactive).await
        }
        Commands::InitProject { cwd } => cli::admin::init_project(cwd),
        Commands::UpgradeProject { cwd } => cli::admin::upgrade_project(cwd),
        Commands::Serve => cli::daemon::serve(cfg).await,
        Commands::Daemon => cli::daemon::daemon(cfg).await,
        Commands::Statistics => cli::statistics::run(cfg).await,
        Commands::Reindex { force } => cli::reindex::run(cfg, force).await,
        Commands::Context { cwd, depth } => cli::context::run(cfg, cwd, depth).await,
        Commands::BugGate {
            cwd,
            base,
            warn_only,
            limit,
        } => cli::bug_gate::run(cfg, cwd, base, warn_only, limit).await,
        Commands::Analyze {
            job,
            similarity_threshold,
            similarity_top_k,
            min_cluster_size,
            num_clusters,
            fuzziness,
        } => {
            cli::analyze::run(
                cfg,
                job,
                similarity_threshold,
                similarity_top_k,
                min_cluster_size,
                num_clusters,
                fuzziness,
            )
            .await
        }
        Commands::Tool {
            name,
            args,
            json,
            schema,
        } => cli::tool::run(cfg, name, args, json, schema).await,
        Commands::Results { kind, limit } => cli::results::run(cfg, kind, limit).await,
        Commands::ImportAdvisories { path } => cli::import_advisories::run(cfg, path).await,
        Commands::Experiment { sub } => cli::experiment::run(cfg, sub).await,
        Commands::Ledger { sub } => cli::ledger::run(cfg, sub).await,
        Commands::A2aAdapter {
            kind,
            port,
            name,
            register_with,
            pi_provider,
            pi_model,
        } => cli::a2a_adapter::run(kind, port, name, register_with, pi_provider, pi_model).await,
        Commands::Status { model, json } => cli::status::run(cfg, model, json).await,
        Commands::TrainLink {
            pairs,
            hidden_size,
            output,
            epochs,
            learning_rate,
            seed,
            signature,
        } => {
            cli::train_link::run(
                pairs,
                hidden_size,
                output,
                epochs,
                learning_rate,
                seed,
                signature,
            )
            .await
        }
        Commands::RmasLoop {
            pattern,
            query,
            backbone,
            rounds,
            n_specialists,
            link_dir,
            max_new_tokens,
            backbones,
        } => {
            cli::rmas_loop::run(
                pattern,
                query,
                backbone,
                rounds,
                n_specialists,
                link_dir,
                max_new_tokens,
                backbones,
            )
            .await
        }
        Commands::TrainOuterLink {
            pairs,
            src_size,
            tgt_size,
            output,
            epochs,
            learning_rate,
            seed,
            signature,
        } => {
            cli::train_outer_link::run(
                pairs,
                src_size,
                tgt_size,
                output,
                epochs,
                learning_rate,
                seed,
                signature,
            )
            .await
        }
    }
}
