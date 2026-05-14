//! Prometheus HTTP metrics endpoint.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::{Router, extract::State, routing::get};
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
         # HELP pgmcp_files_submitted Total files handed off to the embed-pool index channel; in-flight = submitted - indexed - failed\n\
         # TYPE pgmcp_files_submitted counter\n\
         pgmcp_files_submitted {}\n\
         # HELP pgmcp_files_aborted_fk Files aborted mid-pipeline due to a foreign-key violation (parent row deleted underfoot — typically `pgmcp reindex --force` or external admin SQL); one increment per affected file, not per chunk\n\
         # TYPE pgmcp_files_aborted_fk counter\n\
         pgmcp_files_aborted_fk {}\n\
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
         pgmcp_uptime_seconds {}\n\
         # HELP pgmcp_current_rss_bytes Current resident set size in bytes (sampled every 500ms)\n\
         # TYPE pgmcp_current_rss_bytes gauge\n\
         pgmcp_current_rss_bytes {}\n\
         # HELP pgmcp_peak_rss_bytes Peak resident set size in bytes since daemon start\n\
         # TYPE pgmcp_peak_rss_bytes gauge\n\
         pgmcp_peak_rss_bytes {}\n\
         # HELP pgmcp_heavy_cron_running 1 if a heavy cron body is currently executing, 0 otherwise\n\
         # TYPE pgmcp_heavy_cron_running gauge\n\
         pgmcp_heavy_cron_running {}\n\
         # HELP pgmcp_files_with_null_bytes_stripped Files whose content/chunks had at least one NUL byte stripped before SQL insert (Postgres TEXT rejects 0x00)\n\
         # TYPE pgmcp_files_with_null_bytes_stripped counter\n\
         pgmcp_files_with_null_bytes_stripped {}\n\
         # HELP pgmcp_files_with_content_omitted Files where indexed_files.content was deliberately stored as NULL because the source is recreate-cheap from disk (asymmetric-storage policy)\n\
         # TYPE pgmcp_files_with_content_omitted counter\n\
         pgmcp_files_with_content_omitted {}\n\
         # HELP pgmcp_documents_extraction_oom Document extraction subprocesses (pandoc/pdftotext/ps2ascii) killed by signal (typically rlimit hit or OOM)\n\
         # TYPE pgmcp_documents_extraction_oom counter\n\
         pgmcp_documents_extraction_oom {}\n\
         # HELP pgmcp_documents_ocr_triggered Documents whose pdftotext output fell below the per-page text threshold and were routed through the Tesseract OCR fallback\n\
         # TYPE pgmcp_documents_ocr_triggered counter\n\
         pgmcp_documents_ocr_triggered {}\n\
         # HELP pgmcp_documents_ocr_cache_hits OCR runs skipped because a cached result keyed on the PDF byte-hash was already present in ocr_extractions\n\
         # TYPE pgmcp_documents_ocr_cache_hits counter\n\
         pgmcp_documents_ocr_cache_hits {}\n\
         # HELP pgmcp_documents_ocr_failed OCR runs that failed (pdftoppm/tesseract error, timeout, or empty output); caller falls back to sparse pdftotext output\n\
         # TYPE pgmcp_documents_ocr_failed counter\n\
         pgmcp_documents_ocr_failed {}\n\
         # HELP pgmcp_documents_ocr_pages_processed Cumulative count of PDF pages successfully OCR'd across the daemon's lifetime\n\
         # TYPE pgmcp_documents_ocr_pages_processed counter\n\
         pgmcp_documents_ocr_pages_processed {}\n\
         # HELP pgmcp_read_file_disk_hits read_file MCP tool served content from disk after content_hash verification (fast path for plain-text files)\n\
         # TYPE pgmcp_read_file_disk_hits counter\n\
         pgmcp_read_file_disk_hits {}\n\
         # HELP pgmcp_read_file_disk_hash_mismatches read_file MCP tool saw an on-disk file whose hash didn't match the indexed row (file changed since indexing); fell back to chunks\n\
         # TYPE pgmcp_read_file_disk_hash_mismatches counter\n\
         pgmcp_read_file_disk_hash_mismatches {}\n\
         # HELP pgmcp_read_file_disk_io_errors read_file MCP tool failed to read the on-disk file (missing/permission/encoding); fell back to chunks\n\
         # TYPE pgmcp_read_file_disk_io_errors counter\n\
         pgmcp_read_file_disk_io_errors {}\n\
         # HELP pgmcp_read_file_chunk_stitches read_file MCP tool reconstructed content by joining all file_chunks (slow path, content was NULL and disk fast-path was unavailable or failed)\n\
         # TYPE pgmcp_read_file_chunk_stitches counter\n\
         pgmcp_read_file_chunk_stitches {}\n",
        s.files_indexed.load(Ordering::Relaxed),
        s.files_failed.load(Ordering::Relaxed),
        s.files_submitted.load(Ordering::Relaxed),
        s.files_aborted_fk.load(Ordering::Relaxed),
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
        s.current_rss_bytes.load(Ordering::Relaxed),
        s.peak_rss_bytes.load(Ordering::Relaxed),
        if s.heavy_cron_running.load(Ordering::Relaxed) {
            1
        } else {
            0
        },
        s.files_with_null_bytes_stripped.load(Ordering::Relaxed),
        s.files_with_content_omitted.load(Ordering::Relaxed),
        s.documents_extraction_oom.load(Ordering::Relaxed),
        s.documents_ocr_triggered.load(Ordering::Relaxed),
        s.documents_ocr_cache_hits.load(Ordering::Relaxed),
        s.documents_ocr_failed.load(Ordering::Relaxed),
        s.documents_ocr_pages_processed.load(Ordering::Relaxed),
        s.read_file_disk_hits.load(Ordering::Relaxed),
        s.read_file_disk_hash_mismatches.load(Ordering::Relaxed),
        s.read_file_disk_io_errors.load(Ordering::Relaxed),
        s.read_file_chunk_stitches.load(Ordering::Relaxed),
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
