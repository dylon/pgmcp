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
use rmcp::model::{ClientJsonRpcMessage, ServerJsonRpcMessage};
use rmcp::transport::streamable_http_server::session::{
    ServerSseMessage, SessionId, SessionManager,
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use std::sync::atomic::Ordering;
use tracing::{info, warn};

use crate::config::{self, Config};
use crate::context::SystemContext;
use crate::shutdown::ShutdownCoordinator;
use crate::stats::tracker::StatsTracker;
use crate::{
    api, cron, daemon, daemon_state, db, embed, indexer, logging, mcp, shutdown, stats, work_pool,
};

/// Wrap any [`SessionManager`] so that successful `create_session` /
/// `close_session` calls maintain a live count in
/// `StatsTracker::http_mcp_sessions`. Every other trait method
/// transparently delegates to the wrapped manager — the wrapper does
/// not buffer, cache, or alter messages.
struct CountingSessionManager<M: SessionManager> {
    inner: M,
    stats: Arc<StatsTracker>,
}

impl<M: SessionManager> SessionManager for CountingSessionManager<M> {
    type Error = M::Error;
    type Transport = M::Transport;

    async fn create_session(&self) -> Result<(SessionId, Self::Transport), Self::Error> {
        let pair = self.inner.create_session().await?;
        // Increment only on success — a failed create must not leak count.
        self.stats.http_mcp_sessions.fetch_add(1, Ordering::AcqRel);
        Ok(pair)
    }

    async fn initialize_session(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<ServerJsonRpcMessage, Self::Error> {
        self.inner.initialize_session(id, message).await
    }

    async fn has_session(&self, id: &SessionId) -> Result<bool, Self::Error> {
        self.inner.has_session(id).await
    }

    async fn close_session(&self, id: &SessionId) -> Result<(), Self::Error> {
        let result = self.inner.close_session(id).await;
        if result.is_ok() {
            // saturating-sub to make the counter monotone-bounded if the
            // server somehow over-counts close events.
            let _ = self.stats.http_mcp_sessions.fetch_update(
                Ordering::AcqRel,
                Ordering::Acquire,
                |v| Some(v.saturating_sub(1)),
            );
        }
        result
    }

    async fn create_stream(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<impl futures::Stream<Item = ServerSseMessage> + Send + 'static, Self::Error> {
        self.inner.create_stream(id, message).await
    }

    async fn create_standalone_stream(
        &self,
        id: &SessionId,
    ) -> Result<impl futures::Stream<Item = ServerSseMessage> + Send + 'static, Self::Error> {
        self.inner.create_standalone_stream(id).await
    }

    async fn resume(
        &self,
        id: &SessionId,
        last_event_id: String,
    ) -> Result<impl futures::Stream<Item = ServerSseMessage> + Send + 'static, Self::Error> {
        self.inner.resume(id, last_event_id).await
    }

    async fn accept_message(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<(), Self::Error> {
        self.inner.accept_message(id, message).await
    }
}

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

    // 1b. Embedding-signature consistency probe. Compares the bundled
    // signature for the configured model against whatever this DB last
    // wrote. A mismatch is not fatal — the embedding migration cron
    // re-embeds on upgrade — but it's the operator's only chance to
    // notice a daemon downgrade against a newer-signature database,
    // which would otherwise silently mix vector spaces in queries.
    match embed::model::signature_for_model_name(&config_snapshot.embeddings.model) {
        Ok(bundled) => {
            match cron::embedding_migration::active_embedding_signature(&db_pool).await {
                Ok(stored) if stored == bundled => {
                    info!(
                        signature = bundled,
                        "Embedding signature consistent with DB",
                    );
                }
                Ok(stored) => {
                    warn!(
                        bundled,
                        stored = %stored,
                        model = %config_snapshot.embeddings.model,
                        "Embedding signature mismatch: daemon writes `{bundled}` but DB has `{stored}` from a previous run. \
                         Upgrade direction is self-healing once the embedding-migration cron completes; \
                         downgrade direction silently mixes vector spaces and degrades recall — investigate before relying on semantic queries.",
                    );
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        "Failed to read stored embedding signature; signature consistency unverified",
                    );
                }
            }
        }
        Err(e) => {
            warn!(
                error = %e,
                model = %config_snapshot.embeddings.model,
                "Unknown embedding model in config; signature consistency unverified",
            );
        }
    }

    // 2. Initialize stats tracker
    let stats_tracker = Arc::new(stats::tracker::StatsTracker::new());

    // 2b. Document-extraction tool preflight. Logs once at startup which
    // CLI tools (poppler/ghostscript/pandoc) are available for the
    // document indexing pipeline. Missing tools don't abort the daemon —
    // files of the affected types are skipped at index time and counted
    // via `documents_skipped_no_tool` so missing tools surface in
    // `index_stats`. Per-tool `OnceLock` resolution then avoids
    // re-running `which::which` on the hot path.
    preflight_document_tools();

    // 3. Initialize the three role-specialized work pools.
    //
    // - GeneralPool — unbounded CPU-bound work that's neither GPU nor
    //   cron (parallel betweenness centrality, ad-hoc CPU bursts).
    //   Sized from `[work_pool]` config (defaults: num_cpus).
    // - CronPool — small dedicated pool for cron task bodies. Cron
    //   scheduler dispatches each due closure to this pool so a heavy
    //   `block_on` job doesn't stall light cleanup jobs that fire on the
    //   same scheduler tick. 2 workers is plenty given the existing
    //   shared `heavy_cron_lock` already serializes the heavy quartet.
    //
    // The InferencePool (GPU-bound, file-indexing + query-embed +
    // GPU-FCM) is a different type — `embed::pool::EmbeddingPool` —
    // constructed in step 4 below.
    let general_pool = Arc::new(work_pool::pool::WorkPool::new(
        config_snapshot.work_pool.min_threads,
        config_snapshot.work_pool.resolved_max_threads(),
        config_snapshot.work_pool.resolved_initial_threads(),
        shutdown.terminating_flag(),
    ));
    let cron_pool = Arc::new(work_pool::pool::WorkPool::new(
        1,
        2,
        2,
        shutdown.terminating_flag(),
    ));

    // 5. Start scaling monitor for the general pool with a per-pool RSS
    //    budget. `system.rss_limit_mib = 0` (default) resolves to 80% of
    //    MemAvailable at boot; we split it 50/25/25 across InferencePool /
    //    CronPool / GeneralPool, so GeneralPool gets 25%. Inference and
    //    Cron pools don't run their own monitors today (their concurrency
    //    is fixed at construction); the GeneralPool monitor is the one
    //    that actually adapts to RSS pressure.
    let total_rss_budget = config_snapshot.system.resolved_rss_limit_bytes();
    let general_rss_budget = total_rss_budget / 4; // 25% share
    if total_rss_budget > 0 {
        info!(
            total_rss_budget_mib = total_rss_budget >> 20,
            general_pool_share_mib = general_rss_budget >> 20,
            "Per-pool RSS scaling armed"
        );
    } else {
        info!(
            "RSS-aware scaling disabled (system.rss_limit_mib unset and MemAvailable unreadable)"
        );
    }
    let monitor_pool = Arc::clone(&general_pool);
    let monitor_shutdown = shutdown.terminating_flag();
    let monitor_stats = Arc::clone(&stats_tracker);
    let monitor_handle = std::thread::Builder::new()
        .name("pgmcp-monitor".into())
        .spawn(move || {
            work_pool::monitor::run_scaling_monitor(
                &monitor_pool,
                monitor_shutdown,
                &monitor_stats,
                general_rss_budget,
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
        &config_snapshot.fuzzy,
        &config_snapshot.embeddings,
        tokio::runtime::Handle::current(),
        embed_sender.clone(),
        lifecycle.clone(),
        Arc::clone(&cron_pool),
        Some(Arc::clone(&general_pool)),
    );

    // 8. MCP logging broadcaster + task store (constructed early so the
    // SystemContext below can include them; both are needed by the indexer's
    // log path in addition to the MCP server).
    let log_broadcaster = Arc::new(mcp::logging::LogBroadcaster::new());
    let task_store = Arc::new(mcp::tasks::TaskStore::new());

    // Memory-server Phase 4: construct the optional LLM extractor per
    // config. Disabled by default; logged-and-skipped on construction
    // failure so the daemon never crashes over an optional path.
    let llm_extractor: Option<std::sync::Arc<dyn crate::llm::LlmExtractor>> = {
        let cfg = config.load();
        let backend_str = cfg.memory.extractor.backend.clone();
        match crate::llm::parse_backend_choice(&backend_str) {
            Ok(choice) => match crate::llm::make_extractor(choice) {
                Ok(opt) => opt.map(std::sync::Arc::from),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        backend = %backend_str,
                        "LLM extractor construction failed; Stage B + memory_reflect disabled"
                    );
                    None
                }
            },
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    backend = %backend_str,
                    "LLM extractor backend invalid; Stage B + memory_reflect disabled"
                );
                None
            }
        }
    };

    // 9. Build the SystemContext bundle. One context, shared by the
    // indexer, MCP server, and REST API — Arc-clone per field, no deep copy.
    let system_ctx = SystemContext::production_with_extractor(
        Arc::clone(&cron_db),
        embed::EmbedSource::Pool(query_embedder.clone()),
        Arc::clone(&stats_tracker),
        Arc::clone(&config),
        Arc::clone(&log_broadcaster),
        Arc::clone(&task_store),
        lifecycle.clone(),
        llm_extractor.clone(),
    );

    // 10. Start file watcher + scanner
    let project_overrides: Arc<DashMap<PathBuf, config::ProjectOverride>> =
        Arc::new(DashMap::new());
    let (watcher_cmd_tx, watcher_cmd_rx) = crossbeam_channel::bounded(64);

    let indexer_handle = indexer::event_processor::start_indexing(
        system_ctx.clone(),
        embed_sender,
        shutdown.clone(),
        Arc::clone(&project_overrides),
        watcher_cmd_rx,
        watcher_cmd_tx.clone(),
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

    // 11b. Start the durable telemetry writer (if enabled). Without it, the
    // in-memory counters in StatsTracker still tick over but no rows land in
    // `mcp_tool_calls`. The `instrumented_tool_wrap` helper detects the
    // missing sender and drops rows silently in that case.
    let telemetry_writer_handle = if config_snapshot.metrics.telemetry_db_write_enabled {
        if let Some(pool) = system_ctx.db().pool() {
            Some(stats::telemetry_writer::start_telemetry_writer(
                pool.clone(),
                Arc::clone(&stats_tracker),
                config_snapshot.metrics.clone(),
                shutdown.cancellation_token(),
            ))
        } else {
            tracing::warn!("telemetry writer disabled: DbClient has no PgPool (CLI mode?)");
            None
        }
    } else {
        None
    };

    // 11c. Schedule the daily `telemetry-retention` cron job. Runs every
    // 24h and DELETEs `mcp_tool_calls` rows older than
    // `metrics.telemetry_retention_days` (default 30).
    if config_snapshot.metrics.telemetry_db_write_enabled
        && let Some(pool) = system_ctx.db().pool().cloned()
    {
        let stats_for_retention = Arc::clone(&stats_tracker);
        let retention_days = config_snapshot.metrics.telemetry_retention_days;
        let rt_for_retention = tokio::runtime::Handle::current();
        // 24h interval. Initial delay 30s so we don't run during the
        // startup window when other heavy initialization is in flight.
        cron_handle.schedule_recurring(
            30_000,
            24 * 60 * 60 * 1000,
            "telemetry-retention",
            move || {
                let pool = pool.clone();
                let stats = Arc::clone(&stats_for_retention);
                rt_for_retention.spawn(async move {
                    cron::telemetry_retention::run_or_log(Arc::new(pool), stats, retention_days)
                        .await;
                });
                true
            },
        );
    }

    // 12. Construct the MCP server from the same SystemContext.
    let mcp_server = mcp::server::McpServer::new(system_ctx.clone());

    // 12a. Background-seed the software-pattern catalog so the first MCP
    // pattern-tool call doesn't block on ~1400 chunk embeddings. Lazy
    // seeding remains as a safety net for non-daemon invocations.
    {
        let warm_ctx = system_ctx.clone();
        tokio::spawn(async move {
            match mcp::tools::tool_software_patterns::warm_pattern_catalog(&warm_ctx).await {
                Ok(()) => tracing::info!("Software pattern catalog warm-up complete"),
                Err(e) => tracing::warn!(error = %e, "Software pattern catalog warm-up failed"),
            }
        });
    }

    let cancel_token = shutdown.cancellation_token();

    if is_daemon {
        // Daemon mode: Streamable HTTP transport — multiple clients can connect
        let bind_addr = format!("{}:{}", config_snapshot.mcp.host, config_snapshot.mcp.port);
        info!(
            "Starting MCP server on http://{}/mcp (Streamable HTTP)",
            bind_addr
        );

        // Wrap LocalSessionManager so create/close maintain the
        // http_mcp_sessions counter — surfaced by `pgmcp status` and
        // `/api/status`.
        let counting_manager = CountingSessionManager {
            inner: LocalSessionManager::default(),
            stats: Arc::clone(&stats_tracker),
        };
        let mcp_service = StreamableHttpService::new(
            move || Ok(mcp_server.clone()),
            Arc::new(counting_manager),
            StreamableHttpServerConfig {
                stateful_mode: true,
                cancellation_token: cancel_token.clone(),
                ..Default::default()
            },
        );

        // Memory-server Phase 4: reuse the LLM extractor built earlier
        // (already wired into SystemContext). The REST API gets the same
        // handle so /api/session/observe can fire Stage B.
        let api_llm_extractor = llm_extractor.clone();
        let extractor_debounce: crate::llm::extractor_worker::DebounceMap =
            std::sync::Arc::new(dashmap::DashMap::new());

        // REST API state (shares query_embedder, db, and stats with MCP server)
        let api_state = api::ApiState {
            db: Arc::clone(&cron_db),
            query_embedder: query_embedder.clone(),
            config: Arc::clone(&config),
            stats: Arc::clone(&stats_tracker),
            lifecycle: lifecycle.clone(),
            llm_extractor: api_llm_extractor,
            extractor_debounce,
            system_ctx: system_ctx.clone(),
        };

        let router = axum::Router::new()
            .nest_service("/mcp", mcp_service)
            .route("/health", axum::routing::get(api::handlers::health))
            .route("/api/search", axum::routing::post(api::handlers::search))
            .route("/api/context", axum::routing::get(api::handlers::context))
            .route("/api/mandates", axum::routing::get(api::handlers::mandates))
            .route("/api/status", axum::routing::get(api::handlers::status))
            .route("/api/grep", axum::routing::post(api::handlers::grep))
            .route(
                "/api/file_envelope",
                axum::routing::post(api::handlers::file_envelope),
            )
            .route(
                "/api/session/observe",
                axum::routing::post(api::handlers::session_observe),
            )
            .merge(crate::a2a::a2a_router())
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

    // Drain general pool + cron pool (5s timeout per worker)
    let mut wp_handles = general_pool.shutdown_and_take_handles();
    wp_handles.extend(cron_pool.shutdown_and_take_handles());
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

    // Stop telemetry writer (drains the channel via cancellation token; no
    // hard abort needed, run_telemetry_writer's shutdown branch flushes
    // pending rows before exiting).
    if let Some(handle) = telemetry_writer_handle {
        match tokio::time::timeout(component_timeout, handle).await {
            Ok(Ok(())) => info!("Telemetry writer drained and exited"),
            Ok(Err(e)) => tracing::warn!(error = %e, "Telemetry writer task panicked"),
            Err(_) => tracing::warn!("Telemetry writer did not drain within 5s; aborting"),
        }
    }

    // Close database pool (5s timeout)
    match tokio::time::timeout(component_timeout, db_pool.close()).await {
        Ok(()) => info!("Database pool closed"),
        Err(_) => tracing::warn!("Database pool did not close within 5s"),
    }

    info!("pgmcp shutdown complete");
    Ok(())
}

/// Probe `$PATH` for each CLI tool the document extraction pipeline can
/// use, logging availability once at startup. Missing tools are non-fatal
/// — affected file types are simply skipped at index time and counted via
/// `StatsTracker::documents_skipped_no_tool`. The hint string included
/// with each missing tool tells the operator which package to install.
fn preflight_document_tools() {
    for (tool, langs, hint) in indexer::extract::REQUIRED_TOOLS {
        match which::which(tool) {
            Ok(path) => info!(
                tool = %tool,
                path = %path.display(),
                langs = ?langs,
                "Document extraction tool available"
            ),
            Err(_) => warn!(
                tool = %tool,
                langs = ?langs,
                hint = %hint,
                "Document extraction tool MISSING — files of these types will be skipped"
            ),
        }
    }
}
