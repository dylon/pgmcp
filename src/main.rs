#![recursion_limit = "256"]

extern crate blas_src;
extern crate intel_mkl_src;

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
    /// Print statistics from running instance
    Stats,
    /// Trigger full re-index of all workspaces
    Reindex,
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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
        Commands::Stats => cli::stats::run(cfg).await,
        Commands::Reindex => cli::reindex::run(cfg).await,
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
    }
}
