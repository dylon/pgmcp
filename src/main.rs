#![recursion_limit = "256"]

mod api;
mod cli;
mod config;
mod context;
mod cron;
mod daemon;
mod daemon_state;
mod db;
mod embed;
mod error;
mod fcm;
mod graph;
mod indexer;
mod logging;
mod mcp;
#[allow(dead_code)]
mod mmap_array;
mod reactive;
mod shutdown;
mod stats;
#[allow(dead_code)]
mod topic_store;
mod work_pool;

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
        Commands::Status { model, json } => cli::status::run(cfg, model, json).await,
    }
}
