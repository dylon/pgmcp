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

use arc_swap::ArcSwap;
use clap::{Parser, Subcommand};
use tracing::info;

use rmcp::ServiceExt;

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

        Commands::Serve => {
            let config = Config::load(cli.config.as_deref())?;
            logging::init_foreground(&config);
            info!("pgmcp starting in foreground mode");
            run_server(config, false).await?;
        }

        Commands::Daemon => {
            let config = Config::load(cli.config.as_deref())?;
            logging::init_daemon(&config);
            info!("pgmcp starting in daemon mode");
            daemon::notify_ready();
            run_server(config, true).await?;
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
            sqlx::query("DELETE FROM file_chunks")
                .execute(&pool)
                .await?;
            sqlx::query("DELETE FROM indexed_files")
                .execute(&pool)
                .await?;
            println!("Index cleared. Restart pgmcp to re-index.");
        }
    }

    Ok(())
}

async fn run_server(config: Config, _is_daemon: bool) -> anyhow::Result<()> {
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

    let config_snapshot = config.load();

    // 1. Initialize database
    let db_pool = db::pool::create_pool(&config_snapshot.database).await?;
    db::migrations::run_migrations(&db_pool).await?;
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
    std::thread::Builder::new()
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
    )?;

    // 7. Start cron scheduler
    let (cron_handle, cron_thread, cron_ready) = cron::scheduler::spawn_cron(
        shutdown.terminating_flag(),
    );
    cron_ready
        .recv()
        .expect("Cron scheduler failed to start");

    // Schedule cron jobs
    cron::scheduler::schedule_maintenance_jobs(
        &cron_handle,
        db_pool.clone(),
        Arc::clone(&stats_tracker),
        &config_snapshot.cron,
    );

    // 8. Start file watcher + scanner
    let indexer_handle = indexer::event_processor::start_indexing(
        Arc::clone(&config),
        db_pool.clone(),
        Arc::clone(&work_pool),
        embed_pool.sender(),
        Arc::clone(&stats_tracker),
        shutdown.clone(),
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

    // 10. Start MCP server (blocks until shutdown)
    info!("Starting MCP server on stdio");
    let mcp_server = mcp::server::McpServer::new(
        db_pool.clone(),
        embed_model,
        Arc::clone(&stats_tracker),
        Arc::clone(&config),
    );

    let mcp_service = mcp_server
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| anyhow::anyhow!("MCP server error: {:?}", e))?;

    // Wait for MCP service to finish (client disconnected) or shutdown signal
    let cancel_token = shutdown.cancellation_token();
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

    // Orderly shutdown
    info!("Beginning orderly shutdown...");
    shutdown.signal_shutdown();

    // Stop file watcher
    drop(indexer_handle);

    // Drain work pool
    work_pool.shutdown_and_join();
    info!("Work pool drained");

    // Drain embedding pool
    embed_pool.shutdown();
    info!("Embedding pool drained");

    // Stop cron
    cron_handle.request_shutdown();
    let _ = cron_thread.join();
    info!("Cron scheduler stopped");

    // Stop metrics server
    if let Some(handle) = metrics_handle {
        handle.abort();
    }

    // Close database pool
    db_pool.close().await;
    info!("Database pool closed");

    info!("pgmcp shutdown complete");
    Ok(())
}
