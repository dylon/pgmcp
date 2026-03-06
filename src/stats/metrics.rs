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
