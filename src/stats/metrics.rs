//! Prometheus HTTP metrics endpoint.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::{Router, routing::get, extract::State};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::MetricsConfig;
use crate::stats::tracker::StatsTracker;

#[derive(Clone)]
struct MetricsState {
    stats: Arc<StatsTracker>,
}

async fn metrics_handler(State(state): State<MetricsState>) -> String {
    let s = &state.stats;

    format!(
        "# HELP pgmcp_files_indexed Total files indexed\n\
         # TYPE pgmcp_files_indexed counter\n\
         pgmcp_files_indexed {}\n\
         # HELP pgmcp_files_failed Total files failed to index\n\
         # TYPE pgmcp_files_failed counter\n\
         pgmcp_files_failed {}\n\
         # HELP pgmcp_chunks_embedded Total chunks embedded\n\
         # TYPE pgmcp_chunks_embedded counter\n\
         pgmcp_chunks_embedded {}\n\
         # HELP pgmcp_bytes_processed Total bytes processed\n\
         # TYPE pgmcp_bytes_processed counter\n\
         pgmcp_bytes_processed {}\n\
         # HELP pgmcp_mcp_requests Total MCP requests\n\
         # TYPE pgmcp_mcp_requests counter\n\
         pgmcp_mcp_requests {}\n\
         # HELP pgmcp_mcp_errors Total MCP errors\n\
         # TYPE pgmcp_mcp_errors counter\n\
         pgmcp_mcp_errors {}\n\
         # HELP pgmcp_semantic_searches Total semantic searches\n\
         # TYPE pgmcp_semantic_searches counter\n\
         pgmcp_semantic_searches {}\n\
         # HELP pgmcp_text_searches Total text searches\n\
         # TYPE pgmcp_text_searches counter\n\
         pgmcp_text_searches {}\n\
         # HELP pgmcp_grep_searches Total grep searches\n\
         # TYPE pgmcp_grep_searches counter\n\
         pgmcp_grep_searches {}\n\
         # HELP pgmcp_active_threads Active work pool threads\n\
         # TYPE pgmcp_active_threads gauge\n\
         pgmcp_active_threads {}\n\
         # HELP pgmcp_queue_depth Work pool queue depth\n\
         # TYPE pgmcp_queue_depth gauge\n\
         pgmcp_queue_depth {}\n\
         # HELP pgmcp_cron_executions Total cron task executions\n\
         # TYPE pgmcp_cron_executions counter\n\
         pgmcp_cron_executions {}\n\
         # HELP pgmcp_cron_panics Total cron task panics caught\n\
         # TYPE pgmcp_cron_panics counter\n\
         pgmcp_cron_panics {}\n\
         # HELP pgmcp_git_commits_indexed Total git commits indexed\n\
         # TYPE pgmcp_git_commits_indexed counter\n\
         pgmcp_git_commits_indexed {}\n\
         # HELP pgmcp_git_commits_failed Total git commits failed\n\
         # TYPE pgmcp_git_commits_failed counter\n\
         pgmcp_git_commits_failed {}\n\
         # HELP pgmcp_config_reloads Total successful config reloads\n\
         # TYPE pgmcp_config_reloads counter\n\
         pgmcp_config_reloads {}\n\
         # HELP pgmcp_config_reload_errors Total failed config reload attempts\n\
         # TYPE pgmcp_config_reload_errors counter\n\
         pgmcp_config_reload_errors {}\n\
         # HELP pgmcp_embed_file_batches Total successful file embedding batches\n\
         # TYPE pgmcp_embed_file_batches counter\n\
         pgmcp_embed_file_batches {}\n\
         # HELP pgmcp_embed_commit_batches Total successful commit embedding batches\n\
         # TYPE pgmcp_embed_commit_batches counter\n\
         pgmcp_embed_commit_batches {}\n\
         # HELP pgmcp_embed_errors Total failed embedding calls\n\
         # TYPE pgmcp_embed_errors counter\n\
         pgmcp_embed_errors {}\n\
         # HELP pgmcp_watcher_events_received Total raw file watcher events\n\
         # TYPE pgmcp_watcher_events_received counter\n\
         pgmcp_watcher_events_received {}\n\
         # HELP pgmcp_watcher_events_filtered Total file watcher events passing filters\n\
         # TYPE pgmcp_watcher_events_filtered counter\n\
         pgmcp_watcher_events_filtered {}\n\
         # HELP pgmcp_watcher_events_debounced Total file watcher events after debounce\n\
         # TYPE pgmcp_watcher_events_debounced counter\n\
         pgmcp_watcher_events_debounced {}\n\
         # HELP pgmcp_work_pool_tasks_completed Total work pool tasks completed\n\
         # TYPE pgmcp_work_pool_tasks_completed counter\n\
         pgmcp_work_pool_tasks_completed {}\n\
         # HELP pgmcp_work_pool_scale_ups Total work pool scale-up actions\n\
         # TYPE pgmcp_work_pool_scale_ups counter\n\
         pgmcp_work_pool_scale_ups {}\n\
         # HELP pgmcp_work_pool_scale_downs Total work pool scale-down actions\n\
         # TYPE pgmcp_work_pool_scale_downs counter\n\
         pgmcp_work_pool_scale_downs {}\n\
         # HELP pgmcp_uptime_seconds Server uptime in seconds\n\
         # TYPE pgmcp_uptime_seconds gauge\n\
         pgmcp_uptime_seconds {}\n",
        s.files_indexed.load(Ordering::Relaxed),
        s.files_failed.load(Ordering::Relaxed),
        s.chunks_embedded.load(Ordering::Relaxed),
        s.bytes_processed.load(Ordering::Relaxed),
        s.mcp_requests.load(Ordering::Relaxed),
        s.mcp_errors.load(Ordering::Relaxed),
        s.semantic_searches.load(Ordering::Relaxed),
        s.text_searches.load(Ordering::Relaxed),
        s.grep_searches.load(Ordering::Relaxed),
        s.active_work_pool_threads.load(Ordering::Relaxed),
        s.work_pool_queue_depth.load(Ordering::Relaxed),
        s.cron_executions.load(Ordering::Relaxed),
        s.cron_panics.load(Ordering::Relaxed),
        s.git_commits_indexed.load(Ordering::Relaxed),
        s.git_commits_failed.load(Ordering::Relaxed),
        s.config_reloads.load(Ordering::Relaxed),
        s.config_reload_errors.load(Ordering::Relaxed),
        s.embed_file_batches.load(Ordering::Relaxed),
        s.embed_commit_batches.load(Ordering::Relaxed),
        s.embed_errors.load(Ordering::Relaxed),
        s.watcher_events_received.load(Ordering::Relaxed),
        s.watcher_events_filtered.load(Ordering::Relaxed),
        s.watcher_events_debounced.load(Ordering::Relaxed),
        s.work_pool_tasks_completed.load(Ordering::Relaxed),
        s.work_pool_scale_ups.load(Ordering::Relaxed),
        s.work_pool_scale_downs.load(Ordering::Relaxed),
        s.uptime_start.elapsed().as_secs(),
    )
}

/// Start the Prometheus metrics HTTP server.
pub async fn start_metrics_server(
    config: &MetricsConfig,
    stats: Arc<StatsTracker>,
    cancel: CancellationToken,
) -> anyhow::Result<JoinHandle<()>> {
    let state = MetricsState { stats };
    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(state);

    let bind_addr = format!("{}:{}", config.http_bind, config.http_port);
    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;

    tracing::info!(addr = %bind_addr, "Prometheus metrics server listening");

    let handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                cancel.cancelled().await;
            })
            .await
            .ok();
    });

    Ok(handle)
}
