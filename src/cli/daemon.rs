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
    api, cron, daemon, daemon_state, db, embed, indexer, logging, mcp, proc_clients, shutdown,
    stats, work_pool,
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

/// True when `host` is a loopback bind (same-host only). Used to decide whether
/// to emit the non-loopback security warning before binding the MCP/REST server.
/// Extracted as a pure predicate so it is unit-testable without starting the
/// daemon. `host` is expected pre-trimmed.
fn is_loopback_host(host: &str) -> bool {
    host == "127.0.0.1"
        || host == "::1"
        || host == "localhost"
        || host.eq_ignore_ascii_case("ip6-localhost")
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
    // Retry on transient lock contention (e.g. an orphaned backend from a killed
    // prior instance still holding ACCESS SHARE) instead of aborting startup.
    db::migrations::run_migrations_with_lock_retry(&db_pool, &config_snapshot.vector).await?;
    info!("Database initialized");

    // 1a′. On-disk fuzzy ARTrie format guard. The libdictenstein lock-free overlay
    // refactor changed the trie's on-disk format incompatibly, so any `.artrie`
    // written by a prior binary must be discarded and rebuilt from PG (canonical)
    // rather than mis-read. This wipes `$data_dir/fuzzy/` ONCE on a format-version
    // mismatch, before any trie is opened; the `fuzzy-sync` cron repopulates it.
    match cron::fuzzy_sync::ensure_fuzzy_format_version(&config_snapshot.fuzzy.data_dir) {
        Ok(true) => warn!(
            data_dir = %config_snapshot.fuzzy.data_dir.display(),
            "fuzzy index on-disk format changed (libdictenstein overlay); wiped stale tries — \
             the fuzzy-sync cron will rebuild them from PostgreSQL"
        ),
        Ok(false) => {}
        Err(e) => warn!(error = %e, "fuzzy format-version guard failed (non-fatal; continuing)"),
    }

    // 1b. Log the active embedding signature for operator visibility. The
    // MiniLM/384 path has been removed: BGE-M3 (bge-m3-v1, 1024-d) is the only
    // supported signature and the schema is pinned to it at migration time, so
    // there is no cross-signature state to guard against at startup.
    match crate::embed::signature::read_active_signature(&db_pool).await {
        Ok(sig) => info!(
            signature = sig.as_str(),
            dim = sig.dim(),
            "Active embedding signature"
        ),
        Err(e) => warn!(error = %e, "Failed to read active embedding signature; continuing"),
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
    //
    // GPU admission: when GPU embeddings are enabled, bound the number of
    // resident BGE-M3 copies (pool workers + the embedding-migration cron's
    // transient copy) so they can't exhaust VRAM. The budget is raised to at
    // least `pool_size` so the always-on workers never starve; the headroom
    // above `pool_size` is what the migration cron competes for. See
    // `crate::embed::admission` and `embeddings.gpu_max_resident_embedders`.
    if config_snapshot.embeddings.use_gpu {
        let budget = config_snapshot
            .embeddings
            .gpu_max_resident_embedders
            .max(config_snapshot.embeddings.pool_size);
        if config_snapshot.embeddings.gpu_max_resident_embedders
            < config_snapshot.embeddings.pool_size
        {
            warn!(
                gpu_max_resident_embedders = config_snapshot.embeddings.gpu_max_resident_embedders,
                pool_size = config_snapshot.embeddings.pool_size,
                effective_budget = budget,
                "gpu_max_resident_embedders < pool_size; raising budget to pool_size so \
                 workers don't starve (migration cron then gets no extra slot)"
            );
        }
        embed::admission::init(budget);
    }

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

    // embed_sender + cron_db are defined here because the indexer (below) also
    // consumes them; the cron scheduler is invoked after the SystemContext is
    // built (step 9), since the quality-history cron snapshots GPAs through it.
    let embed_sender = embed_pool.sender();
    let cron_db: Arc<dyn db::DbClient> = Arc::new(db_pool.clone());

    // 8. MCP logging broadcaster + task store (constructed early so the
    // SystemContext below can include them; both are needed by the indexer's
    // log path in addition to the MCP server).
    let log_broadcaster = Arc::new(mcp::logging::LogBroadcaster::new());
    let task_store = Arc::new(mcp::tasks::TaskStore::new());

    // Memory-server Phase 4: construct the optional LLM extractor per
    // config. Disabled by default; logged-and-skipped on construction
    // failure so the daemon never crashes over an optional path.
    // Build the optional LLM extractor in the BACKGROUND and hot-swap it in, so a
    // heavy model load (e.g. Qwen3) never blocks the listener bind. Disabled by
    // default. Readers (memory_reflect, session-observe Stage B, the a2a-reflect
    // and memory-concept crons) see `None` until the load completes — a brief
    // startup window they already handle cleanly, and the crons have minute-scale
    // initial delays that are well past warmup.
    let llm_extractor: Arc<
        parking_lot::RwLock<Option<std::sync::Arc<dyn crate::llm::LlmExtractor>>>,
    > = Arc::new(parking_lot::RwLock::new(None));
    {
        let backend_str = config.load().memory.extractor.backend.clone();
        let slot = Arc::clone(&llm_extractor);
        tokio::task::spawn_blocking(
            move || match crate::llm::parse_backend_choice(&backend_str) {
                Ok(choice) => match crate::llm::make_extractor(choice) {
                    Ok(Some(e)) => {
                        let e: std::sync::Arc<dyn crate::llm::LlmExtractor> =
                            std::sync::Arc::from(e);
                        tracing::info!(backend = %backend_str, "LLM extractor loaded (background)");
                        *slot.write() = Some(e);
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!(
                        error = %e,
                        backend = %backend_str,
                        "LLM extractor construction failed; Stage B + memory_reflect disabled"
                    ),
                },
                Err(e) => tracing::warn!(
                    error = %e,
                    backend = %backend_str,
                    "LLM extractor backend invalid; Stage B + memory_reflect disabled"
                ),
            },
        );
    }

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

    // Schedule cron jobs (heavy jobs gate on lifecycle.is_at_least(Ready)).
    // Invoked after the SystemContext build so the quality-history cron can
    // snapshot project GPAs through the shared context.
    cron::scheduler::schedule_maintenance_jobs(
        &cron_handle,
        Arc::clone(&cron_db),
        Arc::clone(&stats_tracker),
        &config_snapshot.cron,
        &config_snapshot.fuzzy,
        &config_snapshot.embeddings,
        &config_snapshot.clients,
        tokio::runtime::Handle::current(),
        embed_sender.clone(),
        lifecycle.clone(),
        Arc::clone(&cron_pool),
        Some(Arc::clone(&general_pool)),
        system_ctx.clone(),
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

    // 11b-bis. MCP-client OS-identity capture writer. Resolves each connected
    // client's PID/cwd/project from `/proc` (via the TCP peer) and upserts
    // `mcp_clients`, feeding the `active_clients` tool and the A2A
    // active-agents-by-project view. Set the listen port FIRST so `note_client`
    // can disambiguate the client's `/proc/net/tcp` row on the first tool call.
    let _client_writer_handle = if config_snapshot.clients.enabled {
        // Set the listen port FIRST so `note_client` can disambiguate the
        // client's `/proc/net/tcp` row on the first tool call.
        stats_tracker.set_mcp_server_port(config_snapshot.mcp.port);
        if let Some(pool) = system_ctx.db().pool() {
            Some(stats::client_writer::start_client_writer(
                pool.clone(),
                Arc::clone(&stats_tracker),
                shutdown.cancellation_token(),
            ))
        } else {
            tracing::warn!(
                "mcp-client capture writer disabled: DbClient has no PgPool (CLI mode?)"
            );
            None
        }
    } else {
        tracing::info!("mcp-client capture disabled ([clients] enabled = false)");
        None
    };

    // 11b′. Phase-2B eBPF file-event capture (client-agnostic, opt-in). Long-lived
    // task tracing the live client PIDs' openat/open syscalls via bpftrace and
    // recording `ebpf`-source client_file_events. Off by default; needs
    // CAP_BPF+CAP_PERFMON at runtime, so it never affects cap-less hosts.
    let _ebpf_handle = if config_snapshot.clients.enabled && config_snapshot.clients.ebpf_enabled {
        if let Some(pool) = system_ctx.db().pool() {
            Some(proc_clients::ebpf::start_ebpf_consumer(
                pool.clone(),
                config_snapshot.workspace.paths.clone(),
                config_snapshot.clients.ebpf_refresh_secs,
                config_snapshot.clients.ebpf_dedup_secs,
                shutdown.cancellation_token(),
            ))
        } else {
            tracing::warn!("eBPF capture disabled: DbClient has no PgPool (CLI mode?)");
            None
        }
    } else {
        None
    };

    // 11b″. Resilience (src/health): DB-availability breaker prober + disk-space
    // watchdog + ephemeral-event outbox. The prober is the single writer of the
    // shared breaker that lets crons / the embed pool / `/health` short-circuit
    // during a DB outage instead of each eating a 10 s acquire-timeout (the
    // 2026-06-11 ENOSPC incident → 1447 PoolTimedOut lines). The watchdog pauses
    // pgmcp's own disk-growing work and triggers target-cleanup out-of-band under
    // low free bytes / inodes. Both spawn after the pool + migrations + stats are
    // up, so they never probe a not-yet-ready pool.
    let outbox: Option<Arc<crate::health::Outbox>> = if config_snapshot.outbox.enabled {
        let oc = &config_snapshot.outbox;
        crate::health::Outbox::new(
            oc.resolved_dir(),
            oc.max_bytes,
            oc.self_floor_gb.saturating_mul(1 << 30),
            oc.self_floor_inodes,
            crate::health::OnFull::parse(&oc.on_full),
        )
        .map(Arc::new)
    } else {
        tracing::info!("outbox disabled ([outbox] enabled = false)");
        None
    };

    if let Some(pool) = system_ctx.db().pool().cloned() {
        let replayer = outbox.clone().map(|ob| {
            Arc::new(crate::health::OutboxReplayer::new(
                ob,
                &config_snapshot.mcp.host,
                config_snapshot.mcp.port,
                Arc::clone(stats_tracker.db_health()),
            ))
        });
        let _db_prober_handle = crate::health::prober::spawn_db_prober(
            pool.clone(),
            Arc::clone(&stats_tracker),
            replayer,
            config_snapshot.database.health_probe_interval_secs,
            config_snapshot.database.health_probe_timeout_secs,
            shutdown.cancellation_token(),
        );
        if config_snapshot.disk_guard.pause_floor_gb > 0 {
            let _disk_watchdog_handle = crate::health::watchdog::spawn_disk_watchdog(
                pool,
                Arc::clone(&stats_tracker),
                Arc::clone(&config),
                shutdown.cancellation_token(),
            );
        } else {
            tracing::info!("disk-watchdog disabled ([disk_guard] pause_floor_gb = 0)");
        }
    } else {
        tracing::warn!("resilience prober/watchdog disabled: DbClient has no PgPool (CLI mode?)");
    }

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

    // 11d. Schedule the cross-agent best-practice reflection cron (Part A
    // phase A4). Off by default ([a2a.reflection] cron_enabled = false):
    // consensus-gates peer outcomes into the shared scope and promotes the
    // strongest agreed practices to durable mandates.
    if config_snapshot.a2a.reflection.cron_enabled
        && let Some(pool) = system_ctx.db().pool().cloned()
    {
        let stats_for_a2a = Arc::clone(&stats_tracker);
        let extractor_for_a2a = llm_extractor.clone();
        let a2a_cfg = config_snapshot.a2a.reflection.clone();
        let interval_ms = a2a_cfg.cron_interval_secs.saturating_mul(1000);
        let rt_for_a2a = tokio::runtime::Handle::current();
        // 60s initial delay so we don't run during the startup window.
        cron_handle.schedule_recurring(60_000, interval_ms, "a2a-reflect", move || {
            let pool = pool.clone();
            let stats = Arc::clone(&stats_for_a2a);
            let extractor = extractor_for_a2a.read().clone();
            let cfg = a2a_cfg.clone();
            rt_for_a2a.spawn(async move {
                cron::a2a_reflect::run_or_log(Arc::new(pool), stats, extractor, cfg).await;
            });
            true
        });
    }

    // 11d-bis. Schedule the CSM auto-conformance cron (ADR-009). Off by default
    // ([a2a.csm_validate] cron_enabled = false). Validates completed
    // a2a_pattern_* runs with no csm_run_traces row yet, feeding the MSM learner
    // without depending on an agent calling csm_validate_run. LLM-free.
    if config_snapshot.a2a.csm_validate.cron_enabled
        && let Some(pool) = system_ctx.db().pool().cloned()
    {
        let stats_for_csm = Arc::clone(&stats_tracker);
        let csm_cfg = config_snapshot.a2a.csm_validate.clone();
        let interval_ms = csm_cfg.cron_interval_secs.saturating_mul(1000);
        let rt_for_csm = tokio::runtime::Handle::current();
        // 75s initial delay so we don't run during the startup window.
        cron_handle.schedule_recurring(75_000, interval_ms, "csm-validate", move || {
            let pool = pool.clone();
            let stats = Arc::clone(&stats_for_csm);
            let cfg = csm_cfg.clone();
            rt_for_csm.spawn(async move {
                cron::csm_validate::run_or_log(Arc::new(pool), stats, cfg).await;
            });
            true
        });
    }

    // 11d-ter. Schedule the security-scan cron: run installed external security
    // scanners (gitleaks, semgrep, trivy, cargo-audit, …) over each indexed
    // project, persisting findings to external_scanner_findings. Off by default
    // ([security_scan] enabled = false; cron_interval_secs = 0 also disables).
    // The on-demand security_scan MCP tool works regardless of this gate.
    if config_snapshot.security_scan.enabled
        && config_snapshot.security_scan.cron_interval_secs > 0
        && let Some(pool) = system_ctx.db().pool().cloned()
    {
        let sec_cfg = config_snapshot.security_scan.clone();
        let interval_ms = sec_cfg.cron_interval_secs.saturating_mul(1000);
        let rt_for_sec = tokio::runtime::Handle::current();
        // 105s initial delay so we don't run during the startup window.
        cron_handle.schedule_recurring(105_000, interval_ms, "security-scan", move || {
            let pool = pool.clone();
            let cfg = sec_cfg.clone();
            rt_for_sec.spawn(async move {
                cron::security_scan::run_or_log(pool, cfg).await;
            });
            true
        });
    }

    // 11e. Schedule the memory-graph-refresh cron: keep the unified
    // knowledge-graph matviews (memory_unified_nodes + memory_unified_edges)
    // current with the indexed corpus so the traversal tools see fresh nodes/
    // edges. Cheap UNION-ALL projections; default 6h. Set the interval to 0 to
    // disable. (Fixes the previously-never-called refresh path.)
    if config_snapshot.cron.memory_graph_refresh_interval_secs > 0
        && let Some(pool) = system_ctx.db().pool().cloned()
    {
        let stats_for_graph = Arc::clone(&stats_tracker);
        let interval_ms = config_snapshot
            .cron
            .memory_graph_refresh_interval_secs
            .saturating_mul(1000);
        let rt_for_graph = tokio::runtime::Handle::current();
        // 90s initial delay: after the boot-time hash-gated rebuild + warmup.
        cron_handle.schedule_recurring(90_000, interval_ms, "memory-graph-refresh", move || {
            let pool = pool.clone();
            let stats = Arc::clone(&stats_for_graph);
            rt_for_graph.spawn(async move {
                cron::memory_graph_refresh::run_or_log(Arc::new(pool), stats).await;
            });
            true
        });
    }

    // 11f. Schedule the memory-concept-extract cron (Stage 4 auto-population).
    // Off by default ([memory.concepts] cron_enabled = false). Seeds concept
    // entities from code topics (deterministic) + optional LLM-emergent concepts
    // when an extractor is present; refreshes the unified graph at the end.
    if config_snapshot.memory.concepts.cron_enabled
        && let Some(pool) = system_ctx.db().pool().cloned()
    {
        let stats_for_concepts = Arc::clone(&stats_tracker);
        let concepts_cfg = config_snapshot.memory.concepts.clone();
        let extractor_for_concepts = llm_extractor.clone();
        let interval_ms = concepts_cfg.cron_interval_secs.saturating_mul(1000);
        let rt_for_concepts = tokio::runtime::Handle::current();
        // 120s initial delay so it runs after the boot-time graph rebuild.
        cron_handle.schedule_recurring(120_000, interval_ms, "memory-concept-extract", move || {
            let pool = pool.clone();
            let stats = Arc::clone(&stats_for_concepts);
            let cfg = concepts_cfg.clone();
            let extractor = extractor_for_concepts.read().clone();
            rt_for_concepts.spawn(async move {
                cron::memory_concepts::run_or_log(Arc::new(pool), stats, cfg, extractor).await;
            });
            true
        });
    }

    // 11f-bis. Schedule the ontology-invariants cron (Phase 3 invariant mining).
    // Runs by default; set [ontology] cron_interval_secs = 0 to disable. Mines
    // facet='invariant' concepts + evidence from ADRs / mandate files / commits.
    if config_snapshot.ontology.cron_interval_secs > 0
        && let Some(pool) = system_ctx.db().pool().cloned()
    {
        let ontology_cfg = config_snapshot.ontology.clone();
        let interval_ms = ontology_cfg.cron_interval_secs.saturating_mul(1000);
        let rt_for_ontology = tokio::runtime::Handle::current();
        // 180s initial delay so it runs after the boot-time graph rebuild.
        cron_handle.schedule_recurring(180_000, interval_ms, "ontology-invariants", move || {
            let pool = pool.clone();
            let cfg = ontology_cfg.clone();
            rt_for_ontology.spawn(async move {
                cron::ontology_invariants::run_or_log(Arc::new(pool), cfg).await;
            });
            true
        });
    }

    // 11f-ter. Schedule the ontology-build cron (Phase 4 FCA is_a hierarchy).
    // Runs by default (cron_interval_secs > 0). Runs after invariant mining so
    // the concepts it orders already carry facet metadata.
    if config_snapshot.ontology.cron_interval_secs > 0
        && let Some(pool) = system_ctx.db().pool().cloned()
    {
        let ontology_cfg = config_snapshot.ontology.clone();
        let interval_ms = ontology_cfg.cron_interval_secs.saturating_mul(1000);
        let rt_for_ontology_build = tokio::runtime::Handle::current();
        // 240s initial delay so it runs after invariant mining (180s).
        cron_handle.schedule_recurring(240_000, interval_ms, "ontology-build", move || {
            let pool = pool.clone();
            let cfg = ontology_cfg.clone();
            rt_for_ontology_build.spawn(async move {
                cron::ontology_build::run_or_log(Arc::new(pool), cfg).await;
            });
            true
        });
    }

    // 11f-quater. Schedule the ontology-link-predict cron (Phase 8, Poincaré).
    // Runs by default (cron_interval_secs > 0); CPU-only, deterministic (seed 42).
    // Poincaré-embeds the is_a DAG and proposes soft `broader` candidate edges
    // (curator-reviewed — never auto-canonical).
    if config_snapshot.ontology.cron_interval_secs > 0
        && let Some(pool) = system_ctx.db().pool().cloned()
    {
        let ontology_cfg = config_snapshot.ontology.clone();
        let interval_ms = ontology_cfg.cron_interval_secs.saturating_mul(1000);
        let rt_for_ontology_lp = tokio::runtime::Handle::current();
        // 300s initial delay so it runs after the hierarchy build (240s).
        cron_handle.schedule_recurring(300_000, interval_ms, "ontology-link-predict", move || {
            let pool = pool.clone();
            let cfg = ontology_cfg.clone();
            rt_for_ontology_lp.spawn(async move {
                cron::ontology_link_predict::run_or_log(Arc::new(pool), cfg).await;
            });
            true
        });
    }

    // 11f-quinquies. Schedule the ontology-reason cron (Phase 9 constraint check).
    // Runs by default (cron_interval_secs > 0). Recursive-CTE deduction: logs
    // is_a-acyclicity + invariant-anchoring violations; detail via `ontology_check`.
    if config_snapshot.ontology.cron_interval_secs > 0
        && let Some(pool) = system_ctx.db().pool().cloned()
    {
        let ontology_cfg = config_snapshot.ontology.clone();
        let interval_ms = ontology_cfg.cron_interval_secs.saturating_mul(1000);
        let rt_for_ontology_reason = tokio::runtime::Handle::current();
        cron_handle.schedule_recurring(360_000, interval_ms, "ontology-reason", move || {
            let pool = pool.clone();
            let cfg = ontology_cfg.clone();
            rt_for_ontology_reason.spawn(async move {
                cron::ontology_reason::run_or_log(Arc::new(pool), cfg).await;
            });
            true
        });
    }

    // 11f-sexies. Schedule the ontology-migrate cron (Phase 10): fold the
    // software-pattern catalog into the ontology. Runs by default
    // (cron_interval_secs > 0); idempotent, so reruns no-op after the first import.
    if config_snapshot.ontology.cron_interval_secs > 0
        && let Some(pool) = system_ctx.db().pool().cloned()
    {
        let ontology_cfg = config_snapshot.ontology.clone();
        let interval_ms = ontology_cfg.cron_interval_secs.saturating_mul(1000);
        let rt_for_ontology_mig = tokio::runtime::Handle::current();
        // 150s: run before the hierarchy build (240s) so pattern concepts exist.
        cron_handle.schedule_recurring(150_000, interval_ms, "ontology-migrate", move || {
            let pool = pool.clone();
            let cfg = ontology_cfg.clone();
            rt_for_ontology_mig.spawn(async move {
                cron::ontology_migrate::run_or_log(Arc::new(pool), cfg).await;
            });
            true
        });
    }

    // 11f-septies. Schedule the ontology-integrate cron (Phase 11): attach analyzer
    // findings (concurrency v22) as evidence to the concepts governing that code.
    // Runs by default (cron_interval_secs > 0); idempotent.
    if config_snapshot.ontology.cron_interval_secs > 0
        && let Some(pool) = system_ctx.db().pool().cloned()
    {
        let ontology_cfg = config_snapshot.ontology.clone();
        let interval_ms = ontology_cfg.cron_interval_secs.saturating_mul(1000);
        let rt_for_ontology_int = tokio::runtime::Handle::current();
        // 270s: run after the hierarchy build (240s) so concepts + anchors exist.
        cron_handle.schedule_recurring(270_000, interval_ms, "ontology-integrate", move || {
            let pool = pool.clone();
            let cfg = ontology_cfg.clone();
            rt_for_ontology_int.spawn(async move {
                cron::ontology_integrate::run_or_log(Arc::new(pool), cfg).await;
            });
            true
        });
    }

    // 11g. Schedule the trajectory-similarity cron (Stage 5c MSM evolves_like).
    // Off by default ([cron.trajectory_similarity] cron_enabled = false).
    if config_snapshot.cron.trajectory_similarity.cron_enabled
        && let Some(pool) = system_ctx.db().pool().cloned()
    {
        let stats_for_traj = Arc::clone(&stats_tracker);
        let traj_cfg = config_snapshot.cron.trajectory_similarity.clone();
        let interval_ms = traj_cfg.cron_interval_secs.saturating_mul(1000);
        let rt_for_traj = tokio::runtime::Handle::current();
        cron_handle.schedule_recurring(150_000, interval_ms, "trajectory-similarity", move || {
            let pool = pool.clone();
            let stats = Arc::clone(&stats_for_traj);
            let cfg = traj_cfg.clone();
            rt_for_traj.spawn(async move {
                cron::trajectory_similarity::run_or_log(Arc::new(pool), stats, cfg).await;
            });
            true
        });
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

    // 12b. Background-seed the developer-tool ("toolbox") catalog (v32). Same
    // rationale as 12a: the embedding-migration cron backfills the vectors, so
    // this only upserts ~100 compact cards and returns quickly.
    {
        let warm_ctx = system_ctx.clone();
        tokio::spawn(async move {
            match mcp::tools::tool_toolbox::warm_toolbox_catalog(&warm_ctx).await {
                Ok(()) => tracing::info!("Developer-tool catalog warm-up complete"),
                Err(e) => tracing::warn!(error = %e, "Developer-tool catalog warm-up failed"),
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
        // Security posture: the daemon serves mostly-unauthenticated REST + MCP
        // endpoints whose threat model assumes a loopback bind (same-host only).
        // A routable bind exposes them — and the token-gated tracker evidence
        // endpoints — to the network. Warn loudly so a non-loopback bind is a
        // deliberate choice, not an accident.
        {
            let host = config_snapshot.mcp.host.trim();
            if !is_loopback_host(host) {
                tracing::warn!(
                    bind = %bind_addr,
                    tracker_endpoints_gated = config_snapshot.tracker.user_token.is_some(),
                    "MCP/REST server is binding a NON-loopback address; its mostly-unauthenticated \
                     endpoints (search, context, session/observe) and the token-gated tracker \
                     evidence endpoints become network-reachable. Bind 127.0.0.1 unless remote \
                     access is intended and the perimeter is otherwise secured."
                );
            }
        }

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

        // Optional resident reranker for the /api/search hook. Loaded only when
        // [api] rerank_hook is set (the BGE-reranker model is VRAM-exclusive
        // with the Qwen3 extractor). A load failure degrades to RRF-only — the
        // hook still works — so we warn rather than abort.
        // Load the reranker in the BACKGROUND and hot-swap it in, so the
        // (VRAM-exclusive, slow-loading) cross-encoder never blocks the listener
        // bind. Until populated, /api/search uses RRF-only — the existing
        // fallback; a load failure also stays RRF-only.
        let api_reranker: Arc<parking_lot::RwLock<Option<Arc<dyn crate::reranker::Reranker>>>> =
            Arc::new(parking_lot::RwLock::new(None));
        if config.load().api.rerank_hook {
            let slot = Arc::clone(&api_reranker);
            tokio::task::spawn_blocking(move || {
                match crate::reranker::make_reranker(crate::reranker::RerankerChoice::BgeV2M3) {
                    Ok(Some(r)) => {
                        let r: Arc<dyn crate::reranker::Reranker> = Arc::from(r);
                        tracing::info!(
                            reranker = r.name(),
                            "/api/search hook: cross-encoder reranker loaded (background)"
                        );
                        *slot.write() = Some(r);
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!(
                        error = %e,
                        "/api/search hook: reranker load failed; staying RRF-only"
                    ),
                }
            });
        }

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
            reranker: api_reranker,
            outbox: outbox.clone(),
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
            .route(
                "/api/client/file_event",
                axum::routing::post(api::handlers::client_file_event),
            )
            .route(
                "/api/client/inbox_peek",
                axum::routing::post(api::handlers::client_inbox_peek),
            )
            .route(
                "/api/tracker/ingest_plan",
                axum::routing::post(api::handlers::tracker_ingest_plan),
            )
            .route(
                "/api/tracker/record_evidence",
                axum::routing::post(api::handlers::tracker_record_evidence),
            )
            .route(
                "/api/tracker/ci_evidence",
                axum::routing::post(api::handlers::tracker_ci_evidence),
            )
            .route(
                "/api/tracker/pr_event",
                axum::routing::post(api::handlers::tracker_pr_event),
            )
            .route(
                "/api/tracker/project_event",
                axum::routing::post(api::handlers::tracker_project_event),
            )
            .merge(crate::a2a::a2a_router())
            .with_state(api_state);
        let tcp_listener = tokio::net::TcpListener::bind(&bind_addr)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to bind MCP server to {}: {}", bind_addr, e))?;

        if is_daemon {
            daemon::notify_ready();
        }

        // `[a2a] autostart_adapters`: spawn in-process claude + codex A2A leaf
        // adapters so the peer registry is non-empty out of the box (otherwise
        // a2a_list_agents / a2a_pattern_* find nothing). They self-register with
        // this daemon (bounded retry handles the serve-start race); their leaf
        // children run with pgmcp's MCP disabled (see the adapter commands), so
        // they cannot re-enter the pattern tools. Default off.
        if config_snapshot.a2a.autostart_adapters {
            let daemon_url = format!("http://127.0.0.1:{}", config_snapshot.mcp.port);
            for (kind, adapter_port) in [("claude", 3201u16), ("codex", 3202u16)] {
                let url = daemon_url.clone();
                tokio::spawn(async move {
                    if let Err(e) = crate::cli::a2a_adapter::run_embedded(
                        kind.to_string(),
                        adapter_port,
                        None,
                        Some(url),
                    )
                    .await
                    {
                        tracing::warn!(
                            kind, port = adapter_port, error = %e,
                            "autostart a2a-adapter exited"
                        );
                    }
                });
            }
            info!("autostart_adapters: spawned claude (:3201) + codex (:3202) A2A leaf adapters");
        }
        // Time-to-bind marker for the recovery-times harness. The listener is up
        // here; per-request serving-readiness (DB + ≥1 embedder worker) is gated
        // separately (`/health` 200, `/api/search` 503-until-ready), and the
        // initial scan continues in the background (see the `scan_complete`
        // marker). Optional reranker/extractor models load in the background too.
        info!(
            target: "pgmcp::recovery_times",
            phase = "listening",
            addr = %bind_addr,
            "HTTP listener bound — accepting requests"
        );

        // Serve until shutdown signal, with a 5s timeout so SSE connections
        // don't prevent shutdown indefinitely.
        let cancel_for_serve = cancel_token.clone();
        let cancel_for_timeout = cancel_token;

        // Serve with per-connection `ConnectInfo<SocketAddr>` so tool handlers can
        // recover the client's TCP peer (source ip:port) and map it back to the
        // client PID via /proc (see `extract_peer_addr` + `proc_clients`). rmcp's
        // streamable-HTTP tower layer forwards the whole `http::request::Parts`
        // (including its `.extensions`, where axum stores ConnectInfo) into the
        // RequestContext, so nesting `/mcp` does not strip it.
        let serve_future = axum::serve(
            tcp_listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
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
    lifecycle.transition(daemon_state::DaemonPhase::Terminating);
    shutdown.signal_shutdown();

    let component_timeout = Duration::from_secs(5);

    // Reap in-flight heavy-cron backends so they release their table locks NOW,
    // rather than running on (server-side, holding ACCESS SHARE) until
    // statement_timeout after we drop the tokio runtime — which previously
    // orphaned a backend that blocked the *next* daemon's startup migrations
    // (`canceling statement due to lock timeout`). Done before draining the work
    // pools so terminating the query also unblocks the heavy cron's
    // `rt.block_on` worker, letting the drain below finish inside its budget. The
    // pool is still open here (closed further down). Ungraceful death is covered
    // by `client_connection_check_interval` instead; see src/db/admin.rs.
    match tokio::time::timeout(
        component_timeout,
        db::admin::terminate_heavy_backends(&db_pool),
    )
    .await
    {
        Ok(Ok(n)) if n > 0 => info!(terminated = n, "Reaped in-flight heavy-cron backends"),
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "Heavy-backend shutdown sweep failed"),
        Err(_) => tracing::warn!("Heavy-backend shutdown sweep did not complete within 5s"),
    }

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

#[cfg(test)]
mod is_loopback_host_tests {
    use super::is_loopback_host;

    #[test]
    fn loopback_hosts_are_recognized() {
        for h in [
            "127.0.0.1",
            "::1",
            "localhost",
            "ip6-localhost",
            "IP6-LOCALHOST",
        ] {
            assert!(is_loopback_host(h), "{h} should be loopback");
        }
    }

    #[test]
    fn routable_hosts_are_not_loopback() {
        for h in [
            "0.0.0.0",
            "::",
            "192.168.1.10",
            "10.0.0.5",
            "example.com",
            "",
        ] {
            assert!(!is_loopback_host(h), "{h} should NOT be loopback");
        }
    }
}
