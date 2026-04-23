#![recursion_limit = "256"]

extern crate blas_src;
extern crate intel_mkl_src;

mod api;
mod config;
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
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use clap::{Parser, Subcommand};
use dashmap::DashMap;
use tracing::info;

use rmcp::ServiceExt;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};

use crate::config::Config;
use crate::shutdown::ShutdownCoordinator;

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

#[derive(Subcommand, Clone)]
enum AnalyzeJob {
    /// Run only the cross-project similarity scan
    Similarity,
    /// Run only the FCM topic clustering scan (Fuzzy BERTopic)
    Topics,
    /// Run only the graph analysis (import extraction + metrics)
    Graph,
}

#[derive(Subcommand, Clone)]
enum ResultsKind {
    /// Show similarity analysis results
    Similarity,
    /// Show topic clustering results
    Topics,
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

    match cli.command {
        Commands::Init => {
            let path = Config::write_default()?;
            println!("Default configuration written to: {}", path.display());
            return Ok(());
        }

        Commands::UpgradeConfigs { interactive } => {
            // Phase 1: Always upgrade global config
            let global_path = Config::upgrade(cli.config.as_deref())?;
            println!("Global configuration upgraded: {}", global_path.display());

            // Phase 2: Load freshly-upgraded config for DB connection
            let config = Config::load(cli.config.as_deref())?;

            // Phase 3: DB-driven project discovery + upgrade
            println!("Connecting to database for project discovery...");
            match upgrade_all_project_configs(&config.database, interactive).await {
                Ok(()) => {}
                Err(e) => {
                    eprintln!(
                        "Warning: Could not upgrade project configs: {}\n\
                         Use `pgmcp upgrade-project --cwd <DIR>` for individual projects.",
                        e
                    );
                }
            }

            return Ok(());
        }

        Commands::InitProject { cwd } => {
            let project_root = cwd.unwrap_or_else(|| {
                std::env::current_dir().expect("Failed to get current directory")
            });
            let path = config::ProjectOverride::write_default(&project_root)?;
            println!("Project config written to: {}", path.display());
            return Ok(());
        }

        Commands::UpgradeProject { cwd } => {
            let project_root = cwd.unwrap_or_else(|| {
                std::env::current_dir().expect("Failed to get current directory")
            });
            let path = config::ProjectOverride::upgrade(&project_root)?;
            println!("Project config upgraded: {}", path.display());
            return Ok(());
        }

        Commands::Serve => {
            let config_path = Config::resolve_path(cli.config.as_deref());
            let config = Config::load(cli.config.as_deref())?;
            logging::init_foreground(&config);
            info!("pgmcp starting in foreground mode");
            run_server(config, false, config_path).await?;
        }

        Commands::Daemon => {
            let config_path = Config::resolve_path(cli.config.as_deref());
            let config = Config::load(cli.config.as_deref())?;
            logging::init_daemon(&config);
            info!("pgmcp starting in daemon mode");
            run_server(config, true, config_path).await?;
            daemon::notify_stopping();
        }

        Commands::Stats => {
            let config = Config::load(cli.config.as_deref())?;
            stats::cli::print_stats(&config).await?;
        }

        Commands::Reindex => {
            let config = Config::load(cli.config.as_deref())?;
            println!("Triggering full re-index of all workspaces...");
            let pool = db::pool::create_pool(&config.database).await?;
            db::migrations::run_migrations(&pool, &config.vector).await?;
            sqlx::query("DELETE FROM git_commit_chunks")
                .execute(&pool)
                .await?;
            sqlx::query("DELETE FROM git_commits")
                .execute(&pool)
                .await?;
            sqlx::query("DELETE FROM file_chunks")
                .execute(&pool)
                .await?;
            sqlx::query("DELETE FROM indexed_files")
                .execute(&pool)
                .await?;
            // Clear git last commit markers
            sqlx::query("DELETE FROM pgmcp_metadata WHERE key LIKE 'git_last_commit:%'")
                .execute(&pool)
                .await?;
            println!("Index cleared (files + git history). Restart pgmcp to re-index.");
        }

        Commands::Context { cwd, depth } => {
            let config = Config::load(cli.config.as_deref())?;
            let pool = db::pool::create_pool(&config.database).await?;
            run_context_command(&pool, cwd, depth).await?;
        }

        Commands::Analyze {
            job,
            similarity_threshold,
            similarity_top_k,
            min_cluster_size,
            num_clusters,
            fuzziness,
        } => {
            let config = Config::load(cli.config.as_deref())?;
            let pool = db::pool::create_pool(&config.database).await?;
            db::migrations::run_migrations(&pool, &config.vector).await?;

            // Apply CLI overrides to cron config
            let mut cron_config = config.cron.clone();
            if let Some(t) = similarity_threshold {
                cron_config.similarity_threshold = t;
            }
            if let Some(k) = similarity_top_k {
                cron_config.similarity_top_k = k;
            }
            if let Some(s) = min_cluster_size {
                cron_config.topic_min_cluster_size = s;
            }
            if num_clusters.is_some() {
                cron_config.topic_num_clusters = num_clusters;
            }
            if let Some(f) = fuzziness {
                cron_config.topic_fuzziness = f;
            }

            let stats = Arc::new(stats::tracker::StatsTracker::new());

            match job {
                Some(AnalyzeJob::Similarity) => {
                    run_analyze_similarity(&pool, &cron_config, &config.vector, &stats).await;
                }
                Some(AnalyzeJob::Topics) => {
                    run_analyze_topics(&pool, &cron_config, &stats).await;
                }
                Some(AnalyzeJob::Graph) => {
                    run_analyze_graph(&pool, &stats).await;
                }
                None => {
                    run_analyze_similarity(&pool, &cron_config, &config.vector, &stats).await;
                    run_analyze_topics(&pool, &cron_config, &stats).await;
                    run_analyze_graph(&pool, &stats).await;
                }
            }
        }

        Commands::Tool {
            name,
            args,
            json,
            schema,
        } => {
            // Tier 1: list / --schema — no DB, no embed model
            let catalog = mcp::server::McpServer::static_tool_catalog();
            match name {
                None => {
                    list_tools(&catalog);
                    return Ok(());
                }
                Some(ref tool_name) if schema => {
                    show_tool_schema(&catalog, tool_name)?;
                    return Ok(());
                }
                Some(ref tool_name) => {
                    // Tier 2+3: tool execution — DB required, embed model lazy
                    let config = Config::load(cli.config.as_deref())?;
                    let pool = db::pool::create_pool(&config.database).await?;
                    db::migrations::run_migrations(&pool, &config.vector).await?;
                    let stats = Arc::new(stats::tracker::StatsTracker::new());
                    let config_arc = Arc::new(ArcSwap::from_pointee(config));
                    let log_broadcaster = Arc::new(mcp::logging::LogBroadcaster::new());
                    let task_store = Arc::new(mcp::tasks::TaskStore::new());
                    // Lazy embed: no pool running, model created on first embedding tool call
                    let server = mcp::server::McpServer::new(
                        pool,
                        embed::EmbedSource::lazy(config_arc.load().embeddings.clone()),
                        stats,
                        config_arc,
                        log_broadcaster,
                        task_store,
                    );

                    let tool_args = parse_tool_args(&args);
                    match server.call_tool_cli(tool_name, tool_args).await {
                        Ok(result) => {
                            print_tool_result(&result, json);
                            if result.is_error == Some(true) {
                                std::process::exit(1);
                            }
                        }
                        Err(e) => {
                            eprintln!("Error: {}", e.message);
                            std::process::exit(1);
                        }
                    }
                }
            }
        }

        Commands::Results { kind, limit } => {
            let config = Config::load(cli.config.as_deref())?;
            let pool = db::pool::create_pool(&config.database).await?;
            db::migrations::run_migrations(&pool, &config.vector).await?;

            match kind {
                Some(ResultsKind::Similarity) => {
                    print_similarity_results(&pool, limit).await?;
                }
                Some(ResultsKind::Topics) => {
                    print_topic_results(&pool, limit).await?;
                }
                None => {
                    print_similarity_results(&pool, limit).await?;
                    println!();
                    print_topic_results(&pool, limit).await?;
                }
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Analyze helpers
// ---------------------------------------------------------------------------

async fn run_analyze_similarity(
    pool: &sqlx::PgPool,
    cron_config: &config::CronConfig,
    vector_config: &config::VectorConfig,
    stats: &Arc<stats::tracker::StatsTracker>,
) {
    println!(
        "Running similarity scan (threshold={:.2}, top_k={}, ef_search={})...",
        cron_config.similarity_threshold, cron_config.similarity_top_k, vector_config.ef_search,
    );
    let start = std::time::Instant::now();
    cron::similarity::run_similarity_scan(pool, cron_config, vector_config.ef_search, stats).await;
    let elapsed = start.elapsed();
    let pairs = stats
        .similarity_pairs_found
        .load(std::sync::atomic::Ordering::Relaxed);
    println!(
        "Similarity scan complete: {} pairs found in {:.1}s",
        pairs,
        elapsed.as_secs_f64(),
    );
}

async fn run_analyze_topics(
    pool: &sqlx::PgPool,
    cron_config: &config::CronConfig,
    stats: &Arc<stats::tracker::StatsTracker>,
) {
    println!(
        "Running FCM topic clustering (min_cluster_size={}, K={}, m={:.1})...",
        cron_config.topic_min_cluster_size,
        cron_config
            .topic_num_clusters
            .map(|k| k.to_string())
            .unwrap_or_else(|| "auto".into()),
        cron_config.topic_fuzziness,
    );
    let start = std::time::Instant::now();
    cron::topic_clustering::run_global_topic_scan(pool, cron_config, stats).await;
    let elapsed = start.elapsed();
    let topics = stats
        .topics_discovered
        .load(std::sync::atomic::Ordering::Relaxed);
    let noise = stats
        .topic_noise_chunks
        .load(std::sync::atomic::Ordering::Relaxed);
    println!(
        "Topic clustering complete: {} topics, {} noise chunks in {:.1}s",
        topics,
        noise,
        elapsed.as_secs_f64(),
    );
}

async fn run_analyze_graph(pool: &sqlx::PgPool, stats: &Arc<stats::tracker::StatsTracker>) {
    println!("Running graph analysis (import extraction + metrics)...");
    let start = std::time::Instant::now();
    // CLI path: no WorkPool available → sequential Brandes. Daemon path
    // passes Some(work_pool) via schedule_maintenance_jobs for parallel.
    cron::graph_analysis::run_graph_analysis(pool, stats, None).await;
    let elapsed = start.elapsed();
    let runs = stats
        .graph_build_runs
        .load(std::sync::atomic::Ordering::Relaxed);
    println!(
        "Graph analysis complete: {} runs in {:.1}s",
        runs,
        elapsed.as_secs_f64(),
    );
}

// ---------------------------------------------------------------------------
// Tool CLI helpers
// ---------------------------------------------------------------------------

fn parse_tool_args(args: &[String]) -> serde_json::Value {
    use serde_json::{Map, Value};

    let mut map = Map::new();

    for arg in args {
        let (key, val_str) = match arg.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => {
                eprintln!("Warning: ignoring argument without '=': {}", arg);
                continue;
            }
        };

        // Auto-parse the value: try i64 → f64 → bool → string
        let value = if let Ok(n) = val_str.parse::<i64>() {
            Value::Number(n.into())
        } else if let Ok(f) = val_str.parse::<f64>() {
            Value::Number(serde_json::Number::from_f64(f).unwrap_or_else(|| 0.into()))
        } else if val_str == "true" {
            Value::Bool(true)
        } else if val_str == "false" {
            Value::Bool(false)
        } else {
            Value::String(val_str)
        };

        // Repeated keys → array (for Vec<String> params like edge_types, smells)
        if let Some(existing) = map.get_mut(&key) {
            match existing {
                Value::Array(arr) => arr.push(value),
                _ => {
                    let prev = existing.clone();
                    *existing = Value::Array(vec![prev, value]);
                }
            }
        } else {
            map.insert(key, value);
        }
    }

    Value::Object(map)
}

fn list_tools(tools: &[rmcp::model::Tool]) {
    println!("Available pgmcp tools ({} total):", tools.len());
    println!();

    // Group by category: infer from first word/prefix of tool name
    let categories: &[(&str, &[&str])] = &[
        (
            "Search",
            &[
                "semantic_search",
                "text_search",
                "grep",
                "hybrid_search",
                "search_commits",
            ],
        ),
        (
            "File Info",
            &[
                "read_file",
                "project_tree",
                "file_info",
                "list_projects",
                "index_stats",
                "reindex",
            ],
        ),
        (
            "Similarity",
            &[
                "compare_files",
                "find_similar_modules",
                "find_duplicates",
                "refactoring_report",
            ],
        ),
        (
            "Topics",
            &[
                "discover_topics",
                "find_orphans",
                "find_misplaced_code",
                "find_coupled_files",
                "test_coverage_gaps",
                "complexity_hotspots",
                "topic_hierarchy",
                "suggest_merges",
                "suggest_splits",
                "doc_coverage_gaps",
            ],
        ),
        (
            "Graph",
            &[
                "dependency_graph",
                "centrality_analysis",
                "community_detection",
                "circular_dependencies",
                "change_impact_analysis",
            ],
        ),
        (
            "Architecture",
            &[
                "coupling_cohesion_report",
                "architecture_violations",
                "design_smell_detection",
                "architecture_quality",
                "design_metrics",
            ],
        ),
        (
            "Prediction",
            &[
                "bug_prediction",
                "technical_debt_analysis",
                "anomaly_detection",
            ],
        ),
        ("Advanced", &["code_summarize", "engineering_scorecard"]),
    ];

    let tool_map: std::collections::HashMap<&str, &rmcp::model::Tool> =
        tools.iter().map(|t| (t.name.as_ref(), t)).collect();

    for (category, names) in categories {
        let mut found = false;
        for name in *names {
            if let Some(tool) = tool_map.get(name) {
                if !found {
                    println!("  {}:", category);
                    found = true;
                }
                let desc = tool.description.as_deref().unwrap_or("");
                // First sentence only
                let short = desc.split_once(". ").map(|(s, _)| s).unwrap_or(desc);
                let short = if short.len() > 70 {
                    &short[..70]
                } else {
                    short
                };
                println!("    {:<30} {}", name, short);
            }
        }
        if found {
            println!();
        }
    }

    // Show any uncategorized tools
    let categorized: std::collections::HashSet<&str> = categories
        .iter()
        .flat_map(|(_, names)| names.iter().copied())
        .collect();
    let mut uncategorized = false;
    for tool in tools {
        if !categorized.contains(tool.name.as_ref()) {
            if !uncategorized {
                println!("  Other:");
                uncategorized = true;
            }
            let desc = tool.description.as_deref().unwrap_or("");
            let short = desc.split_once(". ").map(|(s, _)| s).unwrap_or(desc);
            let short = if short.len() > 70 {
                &short[..70]
            } else {
                short
            };
            println!("    {:<30} {}", tool.name, short);
        }
    }
    if uncategorized {
        println!();
    }

    println!("Usage: pgmcp tool <name> [KEY=VALUE ...]");
    println!("       pgmcp tool <name> --schema    # show parameter schema");
    println!("       pgmcp tool <name> --json      # compact JSON output");
}

fn show_tool_schema(tools: &[rmcp::model::Tool], name: &str) -> anyhow::Result<()> {
    let tool = tools
        .iter()
        .find(|t| t.name.as_ref() == name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Unknown tool: '{}'. Run `pgmcp tool` to list available tools.",
                name
            )
        })?;

    println!("Tool: {}", tool.name);
    if let Some(desc) = &tool.description {
        println!();
        println!("{}", desc);
    }
    println!();
    println!("Parameters:");
    let schema_json = serde_json::to_string_pretty(&*tool.input_schema)?;
    println!("{}", schema_json);

    Ok(())
}

fn print_tool_result(result: &rmcp::model::CallToolResult, compact: bool) {
    for content in &result.content {
        match &content.raw {
            rmcp::model::RawContent::Text(text_content) => {
                if compact {
                    println!("{}", text_content.text);
                } else {
                    // Try to pretty-print JSON, fallback to raw text
                    match serde_json::from_str::<serde_json::Value>(&text_content.text) {
                        Ok(json) => {
                            if let Ok(pretty) = serde_json::to_string_pretty(&json) {
                                println!("{}", pretty);
                            } else {
                                println!("{}", text_content.text);
                            }
                        }
                        Err(_) => {
                            println!("{}", text_content.text);
                        }
                    }
                }
            }
            _ => {
                eprintln!("[non-text content]");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Results helpers
// ---------------------------------------------------------------------------

fn truncate_path(path: &str, max_len: usize) -> &str {
    if path.len() <= max_len {
        return path;
    }
    // Find a `/` boundary near the start of the tail
    let skip = path.len() - max_len;
    match path[skip..].find('/') {
        Some(pos) => &path[skip + pos..],
        None => &path[skip..],
    }
}

async fn print_similarity_results(pool: &sqlx::PgPool, limit: i32) -> anyhow::Result<()> {
    let total = db::queries::count_similarity_pairs(pool).await?;
    let pairs = db::queries::top_similar_file_pairs(pool, limit).await?;

    println!(
        "=== Cross-Project Similarity ({} total chunk pairs) ===",
        total
    );
    println!();

    if pairs.is_empty() {
        println!("No similarity data found.");
        println!("Run `pgmcp analyze similarity` to populate.");
        return Ok(());
    }

    // Header
    println!(
        "{:<40} {:<40} {:>6} {:>6} {:>6}",
        "File A", "File B", "Avg%", "Max%", "Chunks"
    );
    println!("{}", "-".repeat(100));

    for pair in &pairs {
        let path_a = format!(
            "{}:{}",
            pair.project_name_a,
            truncate_path(&pair.path_a, 30)
        );
        let path_b = format!(
            "{}:{}",
            pair.project_name_b,
            truncate_path(&pair.path_b, 30)
        );
        println!(
            "{:<40} {:<40} {:>5.1}% {:>5.1}% {:>6}",
            truncate_path(&path_a, 40),
            truncate_path(&path_b, 40),
            pair.avg_similarity * 100.0,
            pair.max_similarity * 100.0,
            pair.matching_chunks,
        );
    }

    Ok(())
}

async fn print_topic_results(pool: &sqlx::PgPool, limit: i32) -> anyhow::Result<()> {
    let topics = db::queries::load_cached_topics(pool, "global", limit).await?;

    println!("=== Topic Clustering (global) ===");
    println!();

    if topics.is_empty() {
        println!("No topic data found.");
        println!("Run `pgmcp analyze topics` to populate.");
        return Ok(());
    }

    for (i, topic) in topics.iter().enumerate() {
        let label = topic["label"].as_str().unwrap_or("unknown");
        let size = topic["size"].as_i64().unwrap_or(0);
        let files = topic["files"].as_i64().unwrap_or(0);
        let project_count = topic["project_count"].as_i64().unwrap_or(0);
        let cohesion = topic["avg_internal_similarity"]
            .as_f64()
            .map(|v| format!("{:.1}%", v * 100.0))
            .unwrap_or_else(|| "N/A".into());

        println!(
            "Topic {} — {} ({} chunks, {} files, {} projects, cohesion {})",
            i, label, size, files, project_count, cohesion,
        );

        // Keywords
        if let Some(keywords) = topic["keywords"].as_array() {
            let kw_list: Vec<&str> = keywords.iter().filter_map(|k| k.as_str()).collect();
            if !kw_list.is_empty() {
                println!("  Keywords: {}", kw_list.join(", "));
            }
        }

        // Representative snippet (first 3 lines)
        if let Some(snippet) = topic["representative_snippet"].as_str() {
            let preview: String = snippet
                .lines()
                .take(3)
                .map(|l| format!("  │ {}", l))
                .collect::<Vec<_>>()
                .join("\n");
            if !preview.is_empty() {
                println!("{}", preview);
            }
        }

        // Top files
        if let Some(top_files) = topic["representative_files"].as_array() {
            let file_list: Vec<&str> = top_files
                .iter()
                .filter_map(|f| f.as_str())
                .take(5)
                .collect();
            if !file_list.is_empty() {
                println!(
                    "  Files: {}",
                    file_list
                        .iter()
                        .map(|p| truncate_path(p, 50))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }

        if i < topics.len() - 1 {
            println!();
        }
    }

    Ok(())
}

async fn run_context_command(
    pool: &sqlx::PgPool,
    cwd: Option<PathBuf>,
    depth: i32,
) -> anyhow::Result<()> {
    let cwd_str = match cwd {
        Some(p) => p.to_string_lossy().into_owned(),
        None => std::env::current_dir()?.to_string_lossy().into_owned(),
    };

    // Ensure trailing slash for prefix matching
    let cwd_normalized = if cwd_str.ends_with('/') {
        cwd_str.clone()
    } else {
        format!("{}/", cwd_str)
    };

    match db::queries::find_project_by_cwd(pool, &cwd_normalized).await? {
        Some(project) => {
            let file_count = project.file_count.unwrap_or(0);
            let last_scanned = project
                .last_scanned_at
                .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                .unwrap_or_else(|| "never".into());

            println!("## pgmcp: Project Context for \"{}\"", project.name);
            println!();
            println!(
                "**Root:** {}  |  **Files indexed:** {}  |  **Last scanned:** {}",
                project.path, file_count, last_scanned
            );

            // Language breakdown
            let languages = db::queries::language_summary(pool, &project.name).await?;
            if !languages.is_empty() {
                println!();
                println!("### Languages");
                for lang in &languages {
                    println!("- {}: {} files", lang.language, lang.count);
                }
            }

            // File tree
            let tree = db::queries::project_tree(pool, &project.name, depth).await?;
            if !tree.is_empty() {
                println!();
                println!("### File Tree (depth {})", depth);
                for path in &tree {
                    println!("{}", path);
                }
            }

            println!();
            println!("### Available pgmcp tools");
            println!(
                "Use ToolSearch to load: semantic_search, text_search, grep, read_file, list_projects, project_tree, file_info, index_stats, reindex, search_commits"
            );
            println!();
            println!(
                "**Tip:** Use search_commits for git history. Use semantic_search with project: \"claude\" for past Claude Code sessions/memory."
            );
        }
        None => {
            println!("## pgmcp: No indexed project found for {}", cwd_str);
            println!();
            let projects = db::queries::list_projects(pool).await?;
            if projects.is_empty() {
                println!("No projects are currently indexed.");
            } else {
                println!("### Indexed projects");
                for p in &projects {
                    println!(
                        "- **{}** ({}, {} files)",
                        p.name,
                        p.path,
                        p.file_count.unwrap_or(0)
                    );
                }
            }
            println!();
            println!("### Available pgmcp tools");
            println!(
                "Use ToolSearch to load: semantic_search, text_search, grep, read_file, list_projects, project_tree, file_info, index_stats, reindex, search_commits"
            );
            println!();
            println!(
                "**Tip:** Use search_commits for git history. Use semantic_search with project: \"claude\" for past Claude Code sessions/memory."
            );
        }
    }

    Ok(())
}

async fn upgrade_all_project_configs(
    db_config: &config::DatabaseConfig,
    interactive: bool,
) -> anyhow::Result<()> {
    let pool = db::pool::create_pool(db_config)
        .await
        .map_err(|e| anyhow::anyhow!("Database connection failed: {}", e))?;

    let projects = db::queries::list_projects(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to list projects: {}", e))?;

    if projects.is_empty() {
        println!("No indexed projects found.");
        return Ok(());
    }

    let mut upgraded = 0u32;
    let mut skipped = 0u32;
    let mut failed = 0u32;

    for project in &projects {
        let project_root = std::path::Path::new(&project.path);
        let pgmcp_toml = project_root.join(".pgmcp.toml");

        if !pgmcp_toml.exists() {
            skipped += 1;
            continue;
        }

        if interactive {
            eprint!(
                "Upgrade .pgmcp.toml in {} ({})? [y/N] ",
                project.name, project.path
            );
            use std::io::Write;
            std::io::stderr().flush()?;

            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer)?;
            let answer = answer.trim().to_lowercase();
            if answer != "y" && answer != "yes" {
                println!("  Skipped {} (declined)", project.name);
                skipped += 1;
                continue;
            }
        }

        match config::ProjectOverride::upgrade(project_root) {
            Ok(path) => {
                println!("  Upgraded: {} ({})", project.name, path.display());
                upgraded += 1;
            }
            Err(e) => {
                eprintln!("  Failed: {} ({}): {}", project.name, project.path, e);
                failed += 1;
            }
        }
    }

    println!(
        "\nProject configs: {} upgraded, {} skipped, {} failed",
        upgraded, skipped, failed
    );
    Ok(())
}

async fn run_server(config: Config, is_daemon: bool, config_path: PathBuf) -> anyhow::Result<()> {
    let shutdown = ShutdownCoordinator::new();
    let lifecycle = daemon_state::DaemonLifecycle::new();
    let config = Arc::new(ArcSwap::from_pointee(config));

    // Set up signal handlers
    let shutdown_clone = shutdown.clone();
    tokio::spawn(async move {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to register SIGTERM handler");
        let sigint = tokio::signal::ctrl_c();

        tokio::select! {
            _ = sigterm.recv() => {
                info!("Received SIGTERM, shutting down...");
            }
            _ = sigint => {
                info!("Received SIGINT, shutting down...");
            }
        }

        shutdown_clone.signal_shutdown();
    });

    // Spawn shutdown watchdog — hard safety net if any shutdown step hangs
    let watchdog_shutdown = shutdown.terminating_flag();
    std::thread::Builder::new()
        .name("pgmcp-shutdown-watchdog".into())
        .spawn(move || {
            while !watchdog_shutdown.load(std::sync::atomic::Ordering::Acquire) {
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            std::thread::sleep(std::time::Duration::from_secs(15));
            tracing::error!("Shutdown timed out after 15s, forcing exit");
            std::process::exit(1);
        })
        .expect("Failed to spawn shutdown watchdog thread");

    let config_snapshot = config.load();

    // 1. Initialize database
    let db_pool = db::pool::create_pool(&config_snapshot.database).await?;
    db::migrations::run_migrations(&db_pool, &config_snapshot.vector).await?;
    info!("Database initialized");

    // 2. Initialize stats tracker
    let stats_tracker = Arc::new(stats::tracker::StatsTracker::new());

    // 3. Initialize work pool (embedding model creation moved to embed pool)
    let work_pool = Arc::new(work_pool::pool::WorkPool::new(
        config_snapshot.work_pool.min_threads,
        config_snapshot.work_pool.resolved_max_threads(),
        config_snapshot.work_pool.resolved_initial_threads(),
        shutdown.terminating_flag(),
    ));

    // 5. Start scaling monitor
    let monitor_pool = Arc::clone(&work_pool);
    let monitor_shutdown = shutdown.terminating_flag();
    let monitor_stats = Arc::clone(&stats_tracker);
    let monitor_handle = std::thread::Builder::new()
        .name("pgmcp-monitor".into())
        .spawn(move || {
            work_pool::monitor::run_scaling_monitor(
                &monitor_pool,
                monitor_shutdown,
                &monitor_stats,
            );
        })
        .expect("Failed to spawn scaling monitor thread");

    // 5b. Start peak-RSS sampler (Phase 4 observability). Reads
    // /proc/self/statm every 500 ms, writes current + peak into stats_tracker
    // for Prometheus export and per-heavy-cron delta logging.
    let peak_rss_handle = stats::rss::spawn_peak_sampler(
        Arc::clone(&stats_tracker),
        shutdown.terminating_flag(),
        500,
    );

    // 4. Initialize embedding pool
    let embed_pool = embed::pool::EmbeddingPool::new(
        &config_snapshot.embeddings,
        Arc::clone(&stats_tracker),
        shutdown.terminating_flag(),
    )?;
    let query_embedder = embed_pool.query_embedder();

    // 7. Start cron scheduler
    let (cron_handle, cron_thread, cron_ready) = cron::scheduler::spawn_cron(
        shutdown.terminating_flag(),
        Some(Arc::clone(&stats_tracker)),
    );
    cron_ready.recv().expect("Cron scheduler failed to start");

    // Transition lifecycle: initialization complete, about to start scanning
    lifecycle.transition(daemon_state::DaemonPhase::Scanning);

    // Schedule cron jobs (heavy jobs gate on lifecycle.is_at_least(Ready))
    let embed_sender = embed_pool.sender();
    cron::scheduler::schedule_maintenance_jobs(
        &cron_handle,
        db_pool.clone(),
        Arc::clone(&stats_tracker),
        &config_snapshot.cron,
        tokio::runtime::Handle::current(),
        embed_sender.clone(),
        lifecycle.clone(),
        Some(Arc::clone(&work_pool)),
    );

    // 8. Start file watcher + scanner
    let project_overrides: Arc<DashMap<PathBuf, config::ProjectOverride>> =
        Arc::new(DashMap::new());
    let (watcher_cmd_tx, watcher_cmd_rx) = crossbeam_channel::bounded(64);

    let indexer_handle = indexer::event_processor::start_indexing(
        Arc::clone(&config),
        db_pool.clone(),
        Arc::clone(&work_pool),
        embed_sender,
        Arc::clone(&stats_tracker),
        shutdown.clone(),
        Arc::clone(&project_overrides),
        watcher_cmd_rx,
        lifecycle.clone(),
    )?;

    // 8b. Start config file watcher for hot-reload
    let _config_watcher_handle = indexer::config_watcher::start_config_watcher(
        Arc::clone(&config),
        config_path,
        watcher_cmd_tx,
        shutdown.terminating_flag(),
        Arc::clone(&stats_tracker),
    )?;

    // 9. Start metrics HTTP server (if enabled)
    let metrics_handle = if config_snapshot.metrics.http_enabled {
        let handle = stats::metrics::start_metrics_server(
            &config_snapshot.metrics,
            Arc::clone(&stats_tracker),
            shutdown.cancellation_token(),
        )
        .await?;
        Some(handle)
    } else {
        None
    };

    // 10. Create MCP logging broadcaster and task store
    let log_broadcaster = Arc::new(mcp::logging::LogBroadcaster::new());
    let task_store = Arc::new(mcp::tasks::TaskStore::new());

    // 11. Start MCP server
    let mcp_server = mcp::server::McpServer::new(
        db_pool.clone(),
        embed::EmbedSource::Pool(query_embedder.clone()),
        Arc::clone(&stats_tracker),
        Arc::clone(&config),
        Arc::clone(&log_broadcaster),
        Arc::clone(&task_store),
    );

    let cancel_token = shutdown.cancellation_token();

    if is_daemon {
        // Daemon mode: Streamable HTTP transport — multiple clients can connect
        let bind_addr = format!("{}:{}", config_snapshot.mcp.host, config_snapshot.mcp.port);
        info!(
            "Starting MCP server on http://{}/mcp (Streamable HTTP)",
            bind_addr
        );

        let mcp_service = StreamableHttpService::new(
            move || Ok(mcp_server.clone()),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig {
                stateful_mode: true,
                cancellation_token: cancel_token.clone(),
                ..Default::default()
            },
        );

        // REST API state (shares query_embedder and db_pool with MCP server)
        let api_state = api::ApiState {
            db_pool: db_pool.clone(),
            query_embedder: query_embedder.clone(),
            config: Arc::clone(&config),
        };

        let router = axum::Router::new()
            .nest_service("/mcp", mcp_service)
            .route("/api/search", axum::routing::post(api::handlers::search))
            .route("/api/context", axum::routing::get(api::handlers::context))
            .with_state(api_state);
        let tcp_listener = tokio::net::TcpListener::bind(&bind_addr)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to bind MCP server to {}: {}", bind_addr, e))?;

        if is_daemon {
            daemon::notify_ready();
        }

        // Serve until shutdown signal, with a 5s timeout so SSE connections
        // don't prevent shutdown indefinitely.
        let cancel_for_serve = cancel_token.clone();
        let cancel_for_timeout = cancel_token;

        let serve_future = axum::serve(tcp_listener, router).with_graceful_shutdown(async move {
            cancel_for_serve.cancelled().await;
        });

        tokio::select! {
            result = serve_future => {
                result.map_err(|e| anyhow::anyhow!("MCP HTTP server error: {}", e))?;
            }
            _ = async {
                cancel_for_timeout.cancelled().await;
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            } => {
                tracing::warn!("MCP HTTP server did not shut down within 5s, forcing stop");
            }
        }

        info!("MCP HTTP server stopped");
    } else {
        // Foreground mode: stdio transport — single client (debugging)
        info!("Starting MCP server on stdio");

        let mcp_service = mcp_server
            .serve(rmcp::transport::stdio())
            .await
            .map_err(|e| anyhow::anyhow!("MCP server error: {:?}", e))?;

        // Wait for MCP service to finish (client disconnected) or shutdown signal
        tokio::select! {
            result = mcp_service.waiting() => {
                if let Err(e) = result {
                    tracing::warn!("MCP service ended with error: {:?}", e);
                }
                info!("MCP client disconnected");
            }
            _ = cancel_token.cancelled() => {
                info!("Shutdown signal received");
            }
        }
    }

    // Orderly shutdown
    info!("Beginning orderly shutdown...");
    lifecycle.transition(daemon_state::DaemonPhase::Terminating);
    shutdown.signal_shutdown();

    let component_timeout = Duration::from_secs(5);

    // Stop config watcher (must drop before indexer to close watcher_cmd channel)
    drop(_config_watcher_handle);

    // Stop file watcher
    drop(indexer_handle);

    // Drain work pool (5s timeout per worker)
    let wp_handles = work_pool.shutdown_and_take_handles();
    let wp_count = wp_handles.len();
    let mut wp_timed_out = 0;
    for handle in wp_handles {
        match shutdown::join_with_timeout(handle, component_timeout) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::error!("Work pool worker panicked: {:?}", e),
            Err(_) => {
                wp_timed_out += 1;
            }
        }
    }
    if wp_timed_out > 0 {
        tracing::warn!(
            "{}/{} work pool workers did not stop within 5s",
            wp_timed_out,
            wp_count
        );
    } else {
        info!("Work pool drained");
    }

    // Join monitor thread (5s timeout)
    match shutdown::join_with_timeout(monitor_handle, component_timeout) {
        Ok(Ok(())) => info!("Monitor thread stopped"),
        Ok(Err(e)) => tracing::error!("Monitor thread panicked: {:?}", e),
        Err(_) => tracing::warn!("Monitor thread did not stop within 5s"),
    }

    // Join peak-RSS sampler thread (5s timeout)
    match shutdown::join_with_timeout(peak_rss_handle, component_timeout) {
        Ok(Ok(())) => info!("Peak-RSS sampler stopped"),
        Ok(Err(e)) => tracing::error!("Peak-RSS sampler panicked: {:?}", e),
        Err(_) => tracing::warn!("Peak-RSS sampler did not stop within 5s"),
    }

    // Drain embedding pool (5s timeout per worker)
    let embed_handles = embed_pool.shutdown_take_handles();
    let embed_count = embed_handles.len();
    let mut embed_timed_out = 0;
    for handle in embed_handles {
        match shutdown::join_with_timeout(handle, component_timeout) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::error!("Embedding worker panicked: {:?}", e),
            Err(_) => {
                embed_timed_out += 1;
            }
        }
    }
    if embed_timed_out > 0 {
        tracing::warn!(
            "{}/{} embedding workers did not stop within 5s",
            embed_timed_out,
            embed_count
        );
    } else {
        info!("Embedding pool drained");
    }

    // Stop cron (5s timeout)
    cron_handle.request_shutdown();
    match shutdown::join_with_timeout(cron_thread, component_timeout) {
        Ok(Ok(())) => info!("Cron scheduler stopped"),
        Ok(Err(e)) => tracing::error!("Cron thread panicked: {:?}", e),
        Err(_) => tracing::warn!("Cron thread did not stop within 5s"),
    }

    // Stop metrics server
    if let Some(handle) = metrics_handle {
        handle.abort();
    }

    // Close database pool (5s timeout)
    match tokio::time::timeout(component_timeout, db_pool.close()).await {
        Ok(()) => info!("Database pool closed"),
        Err(_) => tracing::warn!("Database pool did not close within 5s"),
    }

    info!("pgmcp shutdown complete");
    Ok(())
}
