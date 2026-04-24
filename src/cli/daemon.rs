//! `serve` and `daemon` subcommands: bring up the full pgmcp daemon
//! (DB pool + cron + indexer + embed pool + MCP server + REST API),
//! plus orderly shutdown.
//!
//! Foreground (`serve`) talks MCP over stdio for a single client, intended
//! for debugging. Daemon (`daemon`) talks MCP over Streamable HTTP for many
//! clients and notifies systemd via `sd-notify`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use dashmap::DashMap;
use rmcp::ServiceExt;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tracing::info;

use crate::config::{self, Config};
use crate::context::SystemContext;
use crate::shutdown::ShutdownCoordinator;
use crate::{
    api, cron, daemon, daemon_state, db, embed, indexer, logging, mcp, shutdown, stats, work_pool,
};

pub async fn serve(config_override: Option<&Path>) -> anyhow::Result<()> {
    let config_path = Config::resolve_path(config_override);
    let config = Config::load(config_override)?;
    logging::init_foreground(&config);
    info!("pgmcp starting in foreground mode");
    run_server(config, false, config_path).await
}

pub async fn daemon(config_override: Option<&Path>) -> anyhow::Result<()> {
    let config_path = Config::resolve_path(config_override);
    let config = Config::load(config_override)?;
    logging::init_daemon(&config);
    info!("pgmcp starting in daemon mode");
    run_server(config, true, config_path).await?;
    daemon::notify_stopping();
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
    let cron_db: Arc<dyn db::DbClient> = Arc::new(db_pool.clone());
    cron::scheduler::schedule_maintenance_jobs(
        &cron_handle,
        Arc::clone(&cron_db),
        Arc::clone(&stats_tracker),
        &config_snapshot.cron,
        tokio::runtime::Handle::current(),
        embed_sender.clone(),
        lifecycle.clone(),
        Some(Arc::clone(&work_pool)),
    );

    // 8. MCP logging broadcaster + task store (constructed early so the
    // SystemContext below can include them; both are needed by the indexer's
    // log path in addition to the MCP server).
    let log_broadcaster = Arc::new(mcp::logging::LogBroadcaster::new());
    let task_store = Arc::new(mcp::tasks::TaskStore::new());

    // 9. Build the SystemContext bundle. One context, shared by the
    // indexer, MCP server, and REST API — Arc-clone per field, no deep copy.
    let system_ctx = SystemContext::production(
        Arc::clone(&cron_db),
        embed::EmbedSource::Pool(query_embedder.clone()),
        Arc::clone(&stats_tracker),
        Arc::clone(&config),
        Arc::clone(&log_broadcaster),
        Arc::clone(&task_store),
    );

    // 10. Start file watcher + scanner
    let project_overrides: Arc<DashMap<PathBuf, config::ProjectOverride>> =
        Arc::new(DashMap::new());
    let (watcher_cmd_tx, watcher_cmd_rx) = crossbeam_channel::bounded(64);

    let indexer_handle = indexer::event_processor::start_indexing(
        system_ctx.clone(),
        Arc::clone(&work_pool),
        embed_sender,
        shutdown.clone(),
        Arc::clone(&project_overrides),
        watcher_cmd_rx,
        lifecycle.clone(),
    )?;

    // 10b. Start config file watcher for hot-reload
    let _config_watcher_handle = indexer::config_watcher::start_config_watcher(
        Arc::clone(&config),
        config_path,
        watcher_cmd_tx,
        shutdown.terminating_flag(),
        Arc::clone(&stats_tracker),
    )?;

    // 11. Start metrics HTTP server (if enabled)
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

    // 12. Construct the MCP server from the same SystemContext.
    let mcp_server = mcp::server::McpServer::new(system_ctx.clone());

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

        // REST API state (shares query_embedder and db with MCP server)
        let api_state = api::ApiState {
            db: Arc::clone(&cron_db),
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
