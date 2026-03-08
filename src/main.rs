mod api;
mod config;
mod cron;
mod daemon;
mod db;
mod embed;
mod error;
mod indexer;
mod logging;
mod mcp;
mod reactive;
mod shutdown;
mod stats;
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
    StreamableHttpServerConfig, StreamableHttpService,
    session::local::LocalSessionManager,
};

use crate::config::Config;
use crate::shutdown::ShutdownCoordinator;

#[derive(Parser)]
#[command(name = "pgmcp", version, about = "PostgreSQL + pgvector MCP File Indexer")]
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
        None => std::env::current_dir()?
            .to_string_lossy()
            .into_owned(),
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
            println!("Use ToolSearch to load: semantic_search, text_search, grep, read_file, list_projects, project_tree, file_info, index_stats, reindex, search_commits");
            println!();
            println!("**Tip:** Use search_commits for git history. Use semantic_search with project: \"claude\" for past Claude Code sessions/memory.");
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
            println!("Use ToolSearch to load: semantic_search, text_search, grep, read_file, list_projects, project_tree, file_info, index_stats, reindex, search_commits");
            println!();
            println!("**Tip:** Use search_commits for git history. Use semantic_search with project: \"claude\" for past Claude Code sessions/memory.");
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
    let config = Arc::new(ArcSwap::from_pointee(config));

    // Set up signal handlers
    let shutdown_clone = shutdown.clone();
    tokio::spawn(async move {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
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

    // 3. Initialize embedding model (for MCP query path)
    let embed_model = Arc::new(tokio::sync::Mutex::new(
        embed::model::create_embedding_model(&config_snapshot.embeddings)?,
    ));

    // 4. Initialize work pool
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

    // 6. Initialize embedding pool
    let embed_pool = embed::pool::EmbeddingPool::new(
        &config_snapshot.embeddings,
        Arc::clone(&stats_tracker),
        shutdown.terminating_flag(),
    )?;

    // 7. Start cron scheduler
    let (cron_handle, cron_thread, cron_ready) = cron::scheduler::spawn_cron(
        shutdown.terminating_flag(),
        Some(Arc::clone(&stats_tracker)),
    );
    cron_ready
        .recv()
        .expect("Cron scheduler failed to start");

    // Schedule cron jobs
    let embed_sender = embed_pool.sender();
    cron::scheduler::schedule_maintenance_jobs(
        &cron_handle,
        db_pool.clone(),
        Arc::clone(&stats_tracker),
        &config_snapshot.cron,
        tokio::runtime::Handle::current(),
        embed_sender.clone(),
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
        Arc::clone(&embed_model),
        Arc::clone(&stats_tracker),
        Arc::clone(&config),
        Arc::clone(&log_broadcaster),
        Arc::clone(&task_store),
    );

    let cancel_token = shutdown.cancellation_token();

    if is_daemon {
        // Daemon mode: Streamable HTTP transport — multiple clients can connect
        let bind_addr = format!(
            "{}:{}",
            config_snapshot.mcp.host, config_snapshot.mcp.port
        );
        info!("Starting MCP server on http://{}/mcp (Streamable HTTP)", bind_addr);

        let mcp_service = StreamableHttpService::new(
            move || Ok(mcp_server.clone()),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig {
                stateful_mode: true,
                cancellation_token: cancel_token.clone(),
                ..Default::default()
            },
        );

        // REST API state (shares embed_model and db_pool with MCP server)
        let api_state = api::ApiState {
            db_pool: db_pool.clone(),
            embed_model: Arc::clone(&embed_model),
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

        let serve_future = axum::serve(tcp_listener, router)
            .with_graceful_shutdown(async move {
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
            Err(_) => { wp_timed_out += 1; }
        }
    }
    if wp_timed_out > 0 {
        tracing::warn!("{}/{} work pool workers did not stop within 5s", wp_timed_out, wp_count);
    } else {
        info!("Work pool drained");
    }

    // Join monitor thread (5s timeout)
    match shutdown::join_with_timeout(monitor_handle, component_timeout) {
        Ok(Ok(())) => info!("Monitor thread stopped"),
        Ok(Err(e)) => tracing::error!("Monitor thread panicked: {:?}", e),
        Err(_) => tracing::warn!("Monitor thread did not stop within 5s"),
    }

    // Drain embedding pool (5s timeout per worker)
    let embed_handles = embed_pool.shutdown_take_handles();
    let embed_count = embed_handles.len();
    let mut embed_timed_out = 0;
    for handle in embed_handles {
        match shutdown::join_with_timeout(handle, component_timeout) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::error!("Embedding worker panicked: {:?}", e),
            Err(_) => { embed_timed_out += 1; }
        }
    }
    if embed_timed_out > 0 {
        tracing::warn!("{}/{} embedding workers did not stop within 5s", embed_timed_out, embed_count);
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
