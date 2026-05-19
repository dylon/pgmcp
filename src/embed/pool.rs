//! Dedicated GPU-bound inference thread pool. Acts as the embedding pool
//! AND, since Step 2a of the candle migration, the file-indexing pipeline
//! pool — each worker reads, hashes, chunks, embeds, and DB-inserts a file
//! end-to-end. The previous WorkPool → bounded(64) channel → EmbedPool
//! dance is gone; one task in, one file fully indexed out.
//!
//! Each embedding thread owns its own `Embedder` (one BertModel + tokenizer
//! bound to one device). No sharing, no inter-worker locks. Two bounded
//! crossbeam channels provide backpressure:
//! - **query channel** (priority): MCP/API query-time embeddings, drained first
//! - **index channel**: file-indexing tasks + git-commit batches

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use arc_swap::ArcSwap;
use crossbeam_channel::{Receiver, Sender, TryRecvError, bounded};
use dashmap::DashMap;
use tracing::{debug, error, info};

use super::model::Embedder;
use crate::config::{Config, EmbeddingsConfig, ProjectOverride};
use crate::db::DbClient;
use crate::error::{PgmcpError, Result};
use crate::indexer::{scanner, watcher};
use crate::stats::tracker::StatsTracker;

/// Remove all NUL (`\0`) bytes from `s` in-place. Returns `true` iff any
/// bytes were removed. Postgres `TEXT` columns reject NUL bytes even
/// though Rust `String` allows them; NUL carries no semantic information
/// in any indexed text format, so stripping is lossless.
fn strip_nul_bytes(s: &mut String) -> bool {
    if s.contains('\0') {
        s.retain(|c| c != '\0');
        true
    } else {
        false
    }
}

/// Decision branch for cross-path content-hash dedup. Computed in the
/// embed worker after content extraction & hashing.
enum DedupAction {
    /// Same content already indexed at this path — Level-2 skip.
    Level2Skip,
    /// Content already indexed at a different path that is now gone
    /// from disk. Update the canonical's path in place; reuse chunks
    /// and embeddings.
    Rename { canonical_id: i64, old_path: String },
    /// Content already indexed at a different path that is still
    /// present. Insert a metadata-only duplicate row pointing at the
    /// canonical; chunk queries dereference via `COALESCE`.
    Duplicate {
        canonical_id: i64,
        canonical_path: String,
    },
    /// No matching content elsewhere — proceed with the normal
    /// extract/chunk/embed/upsert path.
    ProceedNormal,
}

/// Indexing-side request handled by an inference pool worker.
pub enum EmbedIndexRequest {
    /// Full file-indexing pipeline (read → hash → skip-check → chunk →
    /// embed → DB-insert). Replaces the prior split between WorkPool's
    /// `process_file` and the embed pool's `process_file_request`.
    IndexFile(IndexFileTask),
    /// Pre-chunked file embed (legacy; retained for any callers still on
    /// the old API).
    #[allow(dead_code)]
    File(EmbedRequest),
    /// Git commit chunks (still pre-chunked by the cron git-history job).
    Commit(EmbedCommitRequest),
}

/// Drives the full file-indexing pipeline inside an inference-pool worker.
///
/// Carries ARCs of the live config, DB client, and project-resolution
/// state. Each worker resolves the project, applies size/mtime/content
/// skip checks, chunks the file, runs the model forward pass, and inserts
/// chunks — all on its own thread, with no inter-pool channel.
pub struct IndexFileTask {
    pub path: PathBuf,
    pub kind: watcher::FileEventKind,
    pub config: Arc<ArcSwap<Config>>,
    pub db: Arc<dyn DbClient>,
    pub project_roots: Arc<DashMap<PathBuf, scanner::ProjectRoot>>,
    pub project_overrides: Arc<DashMap<PathBuf, ProjectOverride>>,
}

/// Legacy pre-chunked embed request. Retained for callers that haven't
/// migrated to `IndexFileTask`.
#[allow(dead_code)]
pub struct EmbedRequest {
    pub file_id: i64,
    pub chunks: Vec<ChunkData>,
    pub db: Arc<dyn DbClient>,
    pub content_hash: i64,
}

/// A request to embed git commit chunks and store them.
pub struct EmbedCommitRequest {
    /// Commit ID in the database.
    pub commit_id: i64,
    /// Chunk data to embed.
    pub chunks: Vec<ChunkData>,
    /// Database client for upserting.
    pub db: Arc<dyn DbClient>,
}

/// Data for a single chunk to embed.
#[derive(Clone)]
pub struct ChunkData {
    pub chunk_index: i32,
    pub content: String,
    pub start_line: i32,
    pub end_line: i32,
}

/// A query-time embedding request with a oneshot reply channel.
pub struct EmbedQueryRequest {
    pub text: String,
    pub reply: tokio::sync::oneshot::Sender<Result<Vec<f32>>>,
}

/// Cloneable handle for submitting query-time embedding requests.
/// Routes through the embed pool's priority query channel.
#[derive(Clone)]
pub struct QueryEmbedder {
    tx: Sender<EmbedQueryRequest>,
}

impl QueryEmbedder {
    /// Embed a single query string. Returns the embedding vector.
    /// Blocks until a pool worker processes the request.
    pub async fn embed_query(&self, text: String) -> Result<Vec<f32>> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(EmbedQueryRequest {
                text,
                reply: reply_tx,
            })
            .map_err(|_| PgmcpError::Embedding("Embed pool shut down".into()))?;
        reply_rx
            .await
            .map_err(|_| PgmcpError::Embedding("Embed worker dropped reply".into()))?
    }
}

/// Dedicated embedding thread pool with dual channels (query priority + index).
pub struct EmbeddingPool {
    index_tx: Sender<EmbedIndexRequest>,
    query_tx: Sender<EmbedQueryRequest>,
    workers: Vec<JoinHandle<()>>,
}

impl EmbeddingPool {
    /// Create a new embedding pool with the specified number of threads.
    pub fn new(
        config: &EmbeddingsConfig,
        stats: Arc<StatsTracker>,
        shutdown: Arc<AtomicBool>,
    ) -> Result<Self> {
        let pool_size = config.pool_size;
        let batch_size = config.batch_size;
        let (index_tx, index_rx) = bounded::<EmbedIndexRequest>(batch_size * 2);
        let (query_tx, query_rx) = bounded::<EmbedQueryRequest>(8);

        // Capture the tokio runtime handle so embedding workers can run async DB queries.
        let rt_handle = tokio::runtime::Handle::current();

        let mut workers = Vec::with_capacity(pool_size);

        for i in 0..pool_size {
            let index_rx = index_rx.clone();
            let query_rx = query_rx.clone();
            let config = config.clone();
            let stats = Arc::clone(&stats);
            let shutdown = Arc::clone(&shutdown);
            let rt = rt_handle.clone();

            let handle = thread::Builder::new()
                .name(format!("pgmcp-embed-{}", i))
                .spawn(move || {
                    embedding_worker(i, index_rx, query_rx, &config, &stats, &shutdown, &rt);
                })
                .map_err(|e| {
                    PgmcpError::Other(format!("Failed to spawn embedding worker: {}", e))
                })?;

            workers.push(handle);
        }

        info!(pool_size, "Embedding pool started");
        Ok(Self {
            index_tx,
            query_tx,
            workers,
        })
    }

    /// Sender for indexing requests (file chunks, git commits).
    pub fn index_sender(&self) -> Sender<EmbedIndexRequest> {
        self.index_tx.clone()
    }

    /// Backward-compatible alias for `index_sender()`.
    pub fn sender(&self) -> Sender<EmbedIndexRequest> {
        self.index_sender()
    }

    /// Query embedder handle for MCP/API query-time embeddings.
    pub fn query_embedder(&self) -> QueryEmbedder {
        QueryEmbedder {
            tx: self.query_tx.clone(),
        }
    }

    /// Current depth and capacity of the index channel. Used by
    /// observability surfaces to distinguish "stalled" from "back-
    /// pressured" from "draining" — when `(depth, capacity)` reads
    /// `(64, 64)` over consecutive scrapes, workers can't keep up with
    /// scanner submissions.
    // Used externally by future metrics/observability surfaces; not yet
    // wired into the Prometheus exposition (deferred so this PR stays
    // a pure additive data-layer change with no MetricsState surgery).
    #[allow(dead_code)]
    pub fn index_channel_depth(&self) -> (usize, usize) {
        (self.index_tx.len(), self.index_tx.capacity().unwrap_or(0))
    }

    /// Signal shutdown and return worker handles for joining with custom timeout logic.
    pub fn shutdown_take_handles(self) -> Vec<JoinHandle<()>> {
        drop(self.index_tx);
        drop(self.query_tx);
        self.workers
    }

    /// Shutdown the embedding pool and wait for workers to finish.
    #[allow(dead_code)]
    pub fn shutdown(self) {
        for handle in self.shutdown_take_handles() {
            let _ = handle.join();
        }
    }
}

/// Maximum consecutive `Embedder::new` failures before the supervisor
/// gives up on a worker slot. With 1s/2s/4s/.../60s backoff this caps
/// retry duration at roughly 10 minutes — long enough for a transient
/// CUDA reset / weights download to recover, short enough that a
/// permanent fault surfaces in `embed_worker_permanent_failures` rather
/// than spinning forever.
const MAX_CONSECUTIVE_WORKER_FAILURES: u32 = 10;

/// Cap on the supervisor's exponential backoff between retries.
const MAX_WORKER_BACKOFF: std::time::Duration = std::time::Duration::from_secs(60);

/// Supervisor entry point. Constructs the worker's model with retry +
/// exponential backoff so a transient `Embedder::new` failure (driver
/// reset, OOM during model load, transient HF download error) doesn't
/// silently kill the worker slot. Each successful construction runs the
/// event loop until shutdown; if the event loop returns due to channel
/// disconnect (pool shutdown), the supervisor exits without retrying.
fn embedding_worker(
    id: usize,
    index_rx: Receiver<EmbedIndexRequest>,
    query_rx: Receiver<EmbedQueryRequest>,
    config: &EmbeddingsConfig,
    stats: &StatsTracker,
    shutdown: &AtomicBool,
    rt: &tokio::runtime::Handle,
) {
    let mut consecutive_failures: u32 = 0;
    let mut backoff = std::time::Duration::from_secs(1);

    while !shutdown.load(Ordering::Acquire) {
        let model = match Embedder::new(config) {
            Ok(m) => {
                if consecutive_failures > 0 {
                    info!(
                        worker_id = id,
                        attempts = consecutive_failures + 1,
                        "Embedding worker recovered after retry"
                    );
                    stats.embed_worker_restarts.fetch_add(1, Ordering::Relaxed);
                }
                m
            }
            Err(e) => {
                consecutive_failures += 1;
                error!(
                    worker_id = id,
                    error = %e,
                    attempt = consecutive_failures,
                    "Embedder::new failed; supervisor will retry"
                );
                stats.embed_errors.fetch_add(1, Ordering::Relaxed);
                if consecutive_failures >= MAX_CONSECUTIVE_WORKER_FAILURES {
                    error!(
                        worker_id = id,
                        attempts = consecutive_failures,
                        "Embedding worker permanently disabled (exceeded retry budget)"
                    );
                    stats
                        .embed_worker_permanent_failures
                        .fetch_add(1, Ordering::Relaxed);
                    return;
                }
                sleep_with_shutdown(backoff, shutdown);
                backoff = std::cmp::min(backoff.saturating_mul(2), MAX_WORKER_BACKOFF);
                continue;
            }
        };

        stats.embed_workers_alive.fetch_add(1, Ordering::Relaxed);
        debug!(worker_id = id, "Embedding worker started");
        run_worker_event_loop(id, &model, &index_rx, &query_rx, stats, shutdown, rt);
        stats.embed_workers_alive.fetch_sub(1, Ordering::Relaxed);
        debug!(worker_id = id, "Embedding worker exiting");
        // Normal exit means the event loop saw shutdown or a disconnected
        // channel; either way the supervisor is done — no retry.
        return;
    }
}

/// Sleep for at most `dur`, returning early if shutdown is signalled.
/// Polled at 100ms granularity so the daemon's shutdown watchdog isn't
/// blocked by a worker still in its retry backoff.
fn sleep_with_shutdown(dur: std::time::Duration, shutdown: &AtomicBool) {
    let start = std::time::Instant::now();
    while start.elapsed() < dur {
        if shutdown.load(Ordering::Acquire) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100).min(dur - start.elapsed()));
    }
}

fn run_worker_event_loop(
    id: usize,
    model: &Embedder,
    index_rx: &Receiver<EmbedIndexRequest>,
    query_rx: &Receiver<EmbedQueryRequest>,
    stats: &StatsTracker,
    shutdown: &AtomicBool,
    rt: &tokio::runtime::Handle,
) {
    loop {
        if shutdown.load(Ordering::Acquire) {
            break;
        }

        // Priority: drain ALL pending query requests first
        loop {
            match query_rx.try_recv() {
                Ok(req) => process_query_request(model, req, stats),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        // Then select between query (priority) and index channels, with timeout
        crossbeam_channel::select! {
            recv(query_rx) -> msg => {
                match msg {
                    Ok(req) => process_query_request(model, req, stats),
                    Err(_) => return,
                }
            }
            recv(index_rx) -> msg => {
                match msg {
                    Ok(req) => {
                        let start = std::time::Instant::now();
                        match req {
                            EmbedIndexRequest::IndexFile(task) => {
                                process_index_file_task(model, task, stats, rt, id);
                            }
                            EmbedIndexRequest::File(file_req) => {
                                process_file_request(model, file_req, stats, rt, id);
                            }
                            EmbedIndexRequest::Commit(commit_req) => {
                                process_commit_request(model, commit_req, stats, rt, id);
                            }
                        }
                        let elapsed = start.elapsed().as_millis() as u64;
                        stats.embedding_duration_ms.fetch_add(elapsed, Ordering::Relaxed);
                    }
                    Err(_) => return,
                }
            }
            default(std::time::Duration::from_millis(500)) => {}
        }
    }
}

fn process_query_request(model: &Embedder, request: EmbedQueryRequest, stats: &StatsTracker) {
    let result = model.embed(&[&request.text]).and_then(|mut vecs| {
        vecs.pop()
            .ok_or_else(|| PgmcpError::Embedding("No embedding returned".into()))
    });

    if result.is_ok() {
        stats.embed_query_count.fetch_add(1, Ordering::Relaxed);
    } else {
        stats.embed_errors.fetch_add(1, Ordering::Relaxed);
    }

    let _ = request.reply.send(result);
}

fn process_file_request(
    model: &Embedder,
    request: EmbedRequest,
    stats: &StatsTracker,
    rt: &tokio::runtime::Handle,
    worker_id: usize,
) {
    let texts: Vec<&str> = request.chunks.iter().map(|c| c.content.as_str()).collect();

    match model.embed(&texts) {
        Ok(embeddings) => {
            let chunks = request.chunks;
            let db = request.db;
            let file_id = request.file_id;
            let content_hash = request.content_hash;

            rt.block_on(async {
                let mut all_chunks_ok = true;
                for (chunk, embedding) in chunks.iter().zip(embeddings.iter()) {
                    if let Err(e) = db
                        .insert_chunk(
                            file_id,
                            chunk.chunk_index,
                            &chunk.content,
                            chunk.start_line,
                            chunk.end_line,
                            embedding,
                        )
                        .await
                    {
                        error!(
                            file_id,
                            chunk_index = chunk.chunk_index,
                            error = %e,
                            "Failed to insert chunk"
                        );
                        all_chunks_ok = false;
                    }
                }

                if all_chunks_ok && let Err(e) = db.finalize_file_hash(file_id, content_hash).await
                {
                    error!(file_id, error = %e, "Failed to finalize content hash");
                }
            });

            stats
                .chunks_embedded
                .fetch_add(chunks.len() as u64, Ordering::Relaxed);
            stats.embed_file_batches.fetch_add(1, Ordering::Relaxed);
        }
        Err(e) => {
            error!(worker_id, error = %e, "File embedding batch failed");
            stats.embed_errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn process_commit_request(
    model: &Embedder,
    request: EmbedCommitRequest,
    stats: &StatsTracker,
    rt: &tokio::runtime::Handle,
    worker_id: usize,
) {
    let texts: Vec<&str> = request.chunks.iter().map(|c| c.content.as_str()).collect();

    match model.embed(&texts) {
        Ok(embeddings) => {
            let chunks = request.chunks;
            let db = request.db;
            let commit_id = request.commit_id;

            rt.block_on(async {
                for (chunk, embedding) in chunks.iter().zip(embeddings.iter()) {
                    if let Err(e) = db
                        .insert_git_commit_chunk(
                            commit_id,
                            chunk.chunk_index,
                            &chunk.content,
                            embedding,
                        )
                        .await
                    {
                        error!(
                            commit_id,
                            chunk_index = chunk.chunk_index,
                            error = %e,
                            "Failed to insert commit chunk"
                        );
                    }
                }
            });

            stats
                .chunks_embedded
                .fetch_add(chunks.len() as u64, Ordering::Relaxed);
            stats.embed_commit_batches.fetch_add(1, Ordering::Relaxed);
        }
        Err(e) => {
            error!(worker_id, error = %e, "Commit embedding batch failed");
            stats.embed_errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Run the full file-indexing pipeline inside an inference worker.
///
/// Replaces the prior split between `WorkPool → process_file → embed_tx.send`
/// (where chunks travelled through a bounded channel before reaching the
/// embed pool) and the embed pool's `process_file_request` (which only saw
/// pre-chunked input). Now everything runs on one worker thread, holding
/// at most one file's content + chunks at a time.
fn process_index_file_task(
    model: &Embedder,
    task: IndexFileTask,
    stats: &StatsTracker,
    rt: &tokio::runtime::Handle,
    worker_id: usize,
) {
    use chrono::{DateTime, Utc};
    use xxhash_rust::xxh3::xxh3_64;

    use crate::indexer::{chunker, claude_chunker, codex_chunker, document_chunker, extract};

    let path = task.path;
    let path_str = path.to_string_lossy().into_owned();
    let cfg = task.config.load();
    let db = &task.db;

    // Removal: just delete from DB; no read, no embed.
    if matches!(task.kind, watcher::FileEventKind::Remove) {
        if let Err(e) = rt.block_on(db.delete_file(&path_str)) {
            error!(path = %path_str, worker_id, error = %e,
                   "Failed to delete file from index");
            stats.embed_errors.fetch_add(1, Ordering::Relaxed);
        }
        return;
    }

    // Configured-extension check (else silently skip).
    let language = match cfg.indexer.language_for_path(&path) {
        Some(l) => l,
        None => return,
    };

    // Stat the file (sync: fs metadata).
    let metadata = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) => {
            error!(path = %path_str, worker_id, error = %e, "fs::metadata failed");
            stats.files_failed.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    let size_bytes = metadata.len() as i64;
    let modified_at: DateTime<Utc> = metadata
        .modified()
        .map(Into::into)
        .unwrap_or_else(|_| Utc::now());

    // Resolve project (project_root + project_id) — async DB call.
    let resolved = rt.block_on(async {
        match scanner::find_project_root(&path, &task.project_roots) {
            Some((root_path, root_info)) => {
                let info = root_info.clone();
                drop(root_info); // release dashmap ref
                let git_common_dir = crate::indexer::git_indexer::detect_git_common_dir(&root_path);
                let git_root_commits =
                    crate::indexer::git_indexer::detect_git_root_commits(&root_path);
                let id = db
                    .upsert_project(
                        &info.workspace_path,
                        &root_path.to_string_lossy(),
                        &info.name,
                        git_common_dir.as_deref(),
                        git_root_commits.as_deref(),
                    )
                    .await?;
                Ok::<_, sqlx::Error>((
                    id,
                    root_path.to_string_lossy().into_owned(),
                    Some(root_path),
                ))
            }
            None => {
                let workspace = cfg.workspace.paths.first().cloned().unwrap_or_default();
                // The synthetic "default" project has no project_root, so
                // can't run `git rev-parse` — pass NULL for both signals.
                let id = db
                    .upsert_project(&workspace, &workspace, "default", None, None)
                    .await?;
                Ok((id, workspace, None))
            }
        }
    });
    let (project_id, workspace_path, project_root_path) = match resolved {
        Ok(v) => v,
        Err(e) => {
            error!(path = %path_str, worker_id, error = %e, "Failed to upsert project");
            stats.files_failed.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };

    let is_document_language = extract::is_document_language(&language);

    // Per-project size and extraction overrides.
    let project_override_get = |root: &PathBuf| task.project_overrides.get(root);
    let max_file_size_override = project_root_path.as_ref().and_then(|root| {
        project_override_get(root)
            .and_then(|ovr| ovr.indexer.as_ref().and_then(|i| i.max_file_size_bytes))
    });
    let max_doc_source_override = project_root_path.as_ref().and_then(|root| {
        project_override_get(root).and_then(|ovr| {
            ovr.indexer
                .as_ref()
                .and_then(|i| i.max_document_source_bytes)
        })
    });
    let max_extracted_text_override = project_root_path.as_ref().and_then(|root| {
        project_override_get(root).and_then(|ovr| {
            ovr.indexer
                .as_ref()
                .and_then(|i| i.max_extracted_text_bytes)
        })
    });
    let extraction_timeout_override = project_root_path.as_ref().and_then(|root| {
        project_override_get(root).and_then(|ovr| {
            ovr.indexer
                .as_ref()
                .and_then(|i| i.document_extraction_timeout_secs)
        })
    });

    // Document languages get a separate (larger) size cap; code keeps the
    // 1 MiB default. Both still go through the source-byte Level-1 skip
    // below, so unchanged files rescan in O(stat) regardless of size.
    let max_size = if is_document_language {
        max_doc_source_override.unwrap_or(cfg.indexer.max_document_source_bytes)
    } else {
        max_file_size_override.unwrap_or(cfg.indexer.max_file_size_bytes)
    };

    let max_extracted_bytes_value =
        max_extracted_text_override.unwrap_or(cfg.indexer.max_extracted_text_bytes);
    let extraction_rss_bytes = match cfg.indexer.max_extraction_subprocess_rss_bytes {
        0 => None,
        n => Some(n),
    };
    let extract_opts = extract::ExtractOptions {
        timeout: std::time::Duration::from_secs(
            extraction_timeout_override.unwrap_or(cfg.indexer.document_extraction_timeout_secs),
        ),
        max_extracted_bytes: max_extracted_bytes_value,
        max_subprocess_rss_bytes: extraction_rss_bytes,
        ocr: extract::OcrOptions {
            enabled: cfg.indexer.ocr_enabled,
            min_text_chars_per_page: cfg.indexer.ocr_min_text_chars_per_page,
            max_pages: cfg.indexer.ocr_max_pages,
            dpi: cfg.indexer.ocr_dpi,
            languages: cfg.indexer.ocr_languages.clone(),
            total_timeout: std::time::Duration::from_secs(cfg.indexer.ocr_total_timeout_secs),
            // Apportion the extraction byte cap across OCR pages so a single
            // doc cannot blow past the global max_extracted_text_bytes budget.
            max_per_page_bytes: max_extracted_bytes_value
                .checked_div(cfg.indexer.ocr_max_pages.max(1))
                .unwrap_or(1024 * 1024)
                .max(64 * 1024),
            max_subprocess_rss_bytes: extraction_rss_bytes,
        },
    };

    // Pre-read size gate for oversized files: register placeholder, no content.
    if size_bytes > max_size as i64 {
        let mtime_nanos = modified_at.timestamp_nanos_opt().unwrap_or(0);
        let mut hash_buf = [0u8; 16];
        hash_buf[..8].copy_from_slice(&size_bytes.to_le_bytes());
        hash_buf[8..].copy_from_slice(&mtime_nanos.to_le_bytes());
        let content_hash = xxh3_64(&hash_buf) as i64;

        let res = rt.block_on(async {
            if let Ok(Some(existing)) = db.get_content_hash(&path_str).await
                && existing == content_hash
            {
                return Ok::<bool, sqlx::Error>(true); // skipped
            }

            let relative = path
                .strip_prefix(&workspace_path)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();

            let file_id = db
                .upsert_file(
                    project_id,
                    &path_str,
                    &relative,
                    &language,
                    size_bytes,
                    None,
                    Some(content_hash),
                    0,
                    true,
                    false, // content_recoverable_from_disk — irrelevant (oversize placeholder)
                    modified_at,
                )
                .await?;
            db.delete_file_chunks(file_id).await?;
            Ok(false)
        });
        match res {
            Ok(true) => {} // Level-1 size+mtime skip
            Ok(false) => {
                stats.files_indexed.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                error!(path = %path_str, worker_id, error = %e,
                       "Oversized-file registration failed");
                stats.files_failed.fetch_add(1, Ordering::Relaxed);
            }
        }
        return;
    }

    // For PDFs, compute the byte-hash up front so the OCR cache can
    // deduplicate scanned-PDF OCR runs across re-indexes / project clones
    // / HTTP fetches. This is one extra disk read for PDFs only; non-PDF
    // document formats skip the hash and the cache lookup.
    let (ocr_cache_opt, ocr_byte_hash) = if language == "pdf" && cfg.indexer.ocr_enabled {
        match std::fs::read(&path) {
            Ok(bytes) => {
                let hash = xxh3_64(&bytes) as i64;
                let cache = db.pool().map(|p| {
                    Arc::new(extract::ocr_cache::PgOcrCache::new(p.clone(), rt.clone()))
                        as Arc<dyn extract::ocr_cache::OcrCache>
                });
                (cache, Some(hash))
            }
            Err(e) => {
                debug!(
                    path = %path_str,
                    worker_id,
                    error = %e,
                    "fs::read failed for PDF byte-hash; OCR cache disabled for this file"
                );
                (None, None)
            }
        }
    } else {
        (None, None)
    };

    // Read file content (sync). Document languages route through the
    // extraction subprocess pipeline (`pdftotext`, `ps2ascii`, `pandoc`)
    // and end up normalized for token-efficient delivery; code languages
    // are read verbatim as UTF-8.
    let mut content = if is_document_language {
        let cache_ref = ocr_cache_opt.as_deref();
        match extract::extract_for_language_with_cache(
            &language,
            &path,
            &extract_opts,
            cache_ref,
            ocr_byte_hash,
        ) {
            Ok(Some(extracted)) => {
                if extracted.truncated {
                    stats.documents_truncated.fetch_add(1, Ordering::Relaxed);
                }
                extracted.text
            }
            Ok(None) => {
                error!(
                    path = %path_str,
                    worker_id,
                    lang = %language,
                    "Document extractor returned None for known document language"
                );
                stats.files_failed.fetch_add(1, Ordering::Relaxed);
                return;
            }
            Err(extract::ExtractError::ToolMissing { tool }) => {
                debug!(
                    path = %path_str,
                    worker_id,
                    lang = %language,
                    tool,
                    "Skipping document — required tool missing"
                );
                stats
                    .documents_skipped_no_tool
                    .fetch_add(1, Ordering::Relaxed);
                return;
            }
            Err(extract::ExtractError::Timeout) => {
                error!(
                    path = %path_str,
                    worker_id,
                    lang = %language,
                    "Document extraction timed out"
                );
                stats
                    .documents_extraction_timeout
                    .fetch_add(1, Ordering::Relaxed);
                return;
            }
            Err(extract::ExtractError::SubprocessKilled { tool, signal }) => {
                error!(
                    path = %path_str,
                    worker_id,
                    lang = %language,
                    tool,
                    signal,
                    "Document extraction subprocess killed (likely rlimit/OOM); see [indexer] max_extraction_subprocess_rss_bytes"
                );
                stats
                    .documents_extraction_oom
                    .fetch_add(1, Ordering::Relaxed);
                return;
            }
            Err(e) => {
                error!(
                    path = %path_str,
                    worker_id,
                    lang = %language,
                    error = %e,
                    "Document extraction failed"
                );
                stats.files_failed.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
    } else {
        match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                error!(path = %path_str, worker_id, error = %e, "fs::read_to_string failed");
                stats.files_failed.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
    };
    let content_hash = xxh3_64(content.as_bytes()) as i64;

    // Strip NUL bytes before any SQL insert. Postgres `TEXT` columns
    // reject `\0` even though Rust `String` allows them; the OOM-prone
    // `.jsonl` tool-result transcripts have embedded NULs from binary
    // escape sequences. Stripping is lossless because NUL bytes carry
    // no semantic information in any indexed text format. The content
    // hash above is over the ORIGINAL bytes so PR F's disk-fast-path
    // can still verify against the on-disk file.
    let mut had_nul_bytes = strip_nul_bytes(&mut content);

    // Cross-path dedup + rename detection:
    //
    // - Same path + same content_hash → Level-2 skip (caught below).
    // - Different path, same content_hash, old path GONE → rename: update
    //   the canonical's path in place; chunks/embeddings reused.
    // - Different path, same content_hash, old path PRESENT → duplicate:
    //   insert a metadata-only row pointing at the canonical via
    //   `duplicate_of_file_id`; chunk-bearing MCP queries follow the
    //   pointer transparently.
    //
    // For source-code projects this is rare. For document corpora it's
    // the difference between embedding a moved 50-page PDF once or
    // re-embedding it every time the user reorganizes their library.
    let relative_path = path
        .strip_prefix(&workspace_path)
        .unwrap_or(&path)
        .to_string_lossy()
        .into_owned();
    let dedup_action = rt.block_on(async {
        // First — is this row already at this path with this content?
        if let Ok(Some(existing_hash)) = db.get_content_hash(&path_str).await
            && existing_hash == content_hash
        {
            return Ok::<DedupAction, sqlx::Error>(DedupAction::Level2Skip);
        }
        // Otherwise, look for a canonical in the same project with this hash.
        let canonical = db
            .find_canonical_by_content_hash(project_id, content_hash)
            .await?;
        let Some(canonical) = canonical else {
            return Ok(DedupAction::ProceedNormal);
        };
        if canonical.path == path_str {
            // Same row; the Level-2 skip should have caught it. Fall
            // through to the normal upsert (which is idempotent).
            return Ok(DedupAction::ProceedNormal);
        }
        let old_exists = std::path::Path::new(&canonical.path).exists();
        if !old_exists {
            return Ok(DedupAction::Rename {
                canonical_id: canonical.id,
                old_path: canonical.path,
            });
        }
        Ok(DedupAction::Duplicate {
            canonical_id: canonical.id,
            canonical_path: canonical.path,
        })
    });

    match dedup_action {
        Ok(DedupAction::Level2Skip) => return,
        Ok(DedupAction::Rename {
            canonical_id,
            old_path,
        }) => {
            // RENAME — update the existing canonical's path in place.
            let res = rt.block_on(async {
                db.update_file_path_in_place(canonical_id, &path_str, &relative_path, modified_at)
                    .await
            });
            match res {
                Ok(()) => {
                    stats.documents_renamed.fetch_add(1, Ordering::Relaxed);
                    info!(
                        old = %old_path,
                        new = %path_str,
                        canonical_id,
                        "Rename detected — path updated, chunks reused"
                    );
                }
                Err(e) => {
                    error!(path = %path_str, error = %e, "rename update failed");
                    stats.files_failed.fetch_add(1, Ordering::Relaxed);
                }
            }
            return;
        }
        Ok(DedupAction::Duplicate {
            canonical_id,
            canonical_path,
        }) => {
            // DUPLICATE — insert a metadata-only row pointing at the
            // canonical; chunk-bearing queries dereference via COALESCE.
            let res = rt.block_on(async {
                db.insert_duplicate_file(
                    project_id,
                    &path_str,
                    &relative_path,
                    &language,
                    size_bytes,
                    content_hash,
                    canonical_id,
                    modified_at,
                )
                .await
            });
            match res {
                Ok(_) => {
                    stats.documents_deduplicated.fetch_add(1, Ordering::Relaxed);
                    info!(
                        canonical = %canonical_path,
                        duplicate = %path_str,
                        canonical_id,
                        "Duplicate detected — pointer stored, extraction skipped"
                    );
                }
                Err(e) => {
                    error!(path = %path_str, error = %e, "duplicate-pointer insert failed");
                    stats.files_failed.fetch_add(1, Ordering::Relaxed);
                }
            }
            return;
        }
        Ok(DedupAction::ProceedNormal) => {}
        Err(e) => {
            error!(
                path = %path_str,
                error = %e,
                "dedup decision failed; falling through to normal upsert"
            );
            // Fall through — better to over-index than to drop the file.
        }
    }

    // Asymmetric content-storage policy:
    //   Plain-text languages — content is verbatim `fs::read_to_string`,
    //     trivially re-readable from disk. Store `content = NULL` and set
    //     `content_recoverable_from_disk = true`; read_file falls back to
    //     a disk read after content_hash verification.
    //   Document languages — content is normalized output from pandoc /
    //     pdftotext / ps2ascii, expensive to recreate. Store the
    //     extracted text inline; `content_recoverable_from_disk = false`.
    let (stored_content, content_recoverable_from_disk) = if is_document_language {
        (Some(content.as_str()), false)
    } else {
        stats
            .files_with_content_omitted
            .fetch_add(1, Ordering::Relaxed);
        (None, true)
    };

    // Level-2 content-hash skip + upsert if changed.
    let upsert_res = rt.block_on(async {
        if let Ok(Some(existing)) = db.get_content_hash(&path_str).await
            && existing == content_hash
        {
            return Ok::<Option<i64>, sqlx::Error>(None); // skipped
        }
        let line_count = content.lines().count() as i32;
        let file_id = db
            .upsert_file(
                project_id,
                &path_str,
                &relative_path,
                &language,
                size_bytes,
                stored_content,
                None, // hash finalized after chunks land
                line_count,
                false,
                content_recoverable_from_disk,
                modified_at,
            )
            .await?;
        db.delete_file_chunks(file_id).await?;
        Ok(Some(file_id))
    });
    let file_id = match upsert_res {
        Ok(Some(id)) => id,
        Ok(None) => return, // unchanged content; skip
        Err(e) => {
            error!(path = %path_str, worker_id, error = %e, "upsert_file failed");
            stats.files_failed.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };

    // Chunk content with the language-appropriate chunker. JSONL gets
    // record-aware variants for Claude/Codex session transcripts; latex
    // and org get heading-aware chunking (post-pandoc plain text mostly
    // falls back to paragraph mode internally); pdf/postscript/docx/etc.
    // use paragraph-aware chunking; everything else uses the line chunker.
    let mut chunks = if &*language == "jsonl" && claude_chunker::is_claude_session_transcript(&path)
    {
        claude_chunker::chunk_claude_jsonl(&content)
    } else if &*language == "jsonl" && codex_chunker::is_codex_jsonl(&path) {
        codex_chunker::chunk_codex_jsonl(&content)
    } else if &*language == "jsonl" {
        chunker::chunk_jsonl_content(&content)
    } else if matches!(&*language, "latex" | "org" | "rst") {
        document_chunker::chunk_by_heading(&content, &language)
    } else if matches!(
        &*language,
        "pdf" | "postscript" | "docx" | "doc" | "rtf" | "odt" | "epub" | "bibtex"
    ) {
        document_chunker::chunk_paragraphs(&content, document_chunker::DEFAULT_PARAGRAPH_OPTS)
    } else {
        chunker::chunk_content(
            &content,
            cfg.embeddings.chunk_size_lines,
            cfg.embeddings.chunk_overlap_lines,
        )
    };

    // Strip NUL bytes from chunk content. Most cases are already covered
    // by the strip on `content` above, but `claude_chunker` parses JSON
    // strings and can introduce raw `\0` from `" "` escapes in tool
    // results — those would otherwise reject at `insert_chunk` time.
    for chunk in chunks.iter_mut() {
        if strip_nul_bytes(&mut chunk.content) {
            had_nul_bytes = true;
        }
    }
    if had_nul_bytes {
        stats
            .files_with_null_bytes_stripped
            .fetch_add(1, Ordering::Relaxed);
    }

    // Cap chunk content at the per-row tsvector limit. JSONL transcripts
    // occasionally have a single message > 1 MiB; without this the
    // `INSERT INTO file_chunks` would fail with `string is too long for
    // tsvector (X bytes, max 1048575 bytes)`. Split is byte-for-byte
    // lossless along UTF-8 boundaries.
    chunks = chunker::split_oversized_chunks(chunks);

    if chunks.is_empty() {
        if let Err(e) = rt.block_on(db.finalize_file_hash(file_id, content_hash)) {
            error!(file_id, worker_id, error = %e,
                   "finalize_file_hash failed (empty chunks)");
        }
        return;
    }

    // Embed inline (one or more sub-batches inside Embedder::embed).
    let texts: Vec<&str> = chunks.iter().map(|c| c.content.as_str()).collect();
    let embeddings = match model.embed(&texts) {
        Ok(v) => v,
        Err(e) => {
            error!(path = %path_str, worker_id, error = %e, "Embedding failed");
            stats.embed_errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };

    // Insert chunks + finalize hash. Three outcomes per file:
    //   - all_ok: every chunk inserted; finalize the hash; count as indexed.
    //   - aborted_fk: parent row deleted underfoot (PG SQLSTATE 23503);
    //     log once at warn!, increment files_aborted_fk, skip finalize.
    //   - other_err: a non-FK error on at least one chunk; per-chunk
    //     error! already logged; skip finalize, but DO count as
    //     indexed-with-errors so the user notices via files_failed.
    enum InsertOutcome {
        AllOk,
        AbortedFk,
        OtherErr,
    }
    let total_chunks = chunks.len();
    let outcome = rt.block_on(async {
        let mut outcome = InsertOutcome::AllOk;
        for (chunk, embedding) in chunks.iter().zip(embeddings.iter()) {
            match db
                .insert_chunk(
                    file_id,
                    chunk.chunk_index,
                    &chunk.content,
                    chunk.start_line,
                    chunk.end_line,
                    embedding,
                )
                .await
            {
                Ok(()) => {}
                Err(e) => {
                    if is_fk_violation(&e) {
                        // Parent row deleted while we were embedding —
                        // remaining chunks would all fail the same way.
                        // Log once per file and break out.
                        tracing::warn!(
                            path = %path_str,
                            file_id,
                            chunks_total = total_chunks,
                            worker_id,
                            reason = "parent row deleted underfoot — likely \
                                      pgmcp reindex --force or external admin SQL",
                            "insert_chunk aborted (FK violation)"
                        );
                        outcome = InsertOutcome::AbortedFk;
                        break;
                    }
                    error!(file_id, chunk_index = chunk.chunk_index, worker_id, error = %e,
                           "insert_chunk failed");
                    outcome = InsertOutcome::OtherErr;
                }
            }
        }
        if matches!(outcome, InsertOutcome::AllOk)
            && let Err(e) = db.finalize_file_hash(file_id, content_hash).await
        {
            // Finalize after a clean run failed — could be FK (race) or
            // anything else. Use the same FK-vs-other discrimination.
            if is_fk_violation(&e) {
                tracing::warn!(
                    path = %path_str,
                    file_id,
                    worker_id,
                    reason = "parent row deleted underfoot during finalize",
                    "finalize_file_hash aborted (FK violation)"
                );
                outcome = InsertOutcome::AbortedFk;
            } else {
                error!(file_id, worker_id, error = %e, "finalize_file_hash failed");
                outcome = InsertOutcome::OtherErr;
            }
        }
        outcome
    });

    match outcome {
        InsertOutcome::AllOk => {
            stats.files_indexed.fetch_add(1, Ordering::Relaxed);
            stats
                .bytes_processed
                .fetch_add(size_bytes as u64, Ordering::Relaxed);
            stats
                .chunks_embedded
                .fetch_add(total_chunks as u64, Ordering::Relaxed);
            stats.embed_file_batches.fetch_add(1, Ordering::Relaxed);
            debug!(path = %path_str, worker_id, language, "File indexed");
        }
        InsertOutcome::AbortedFk => {
            stats.files_aborted_fk.fetch_add(1, Ordering::Relaxed);
        }
        InsertOutcome::OtherErr => {
            stats.files_failed.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// `true` iff the sqlx error is a Postgres foreign-key-violation
/// (SQLSTATE `23503`). Used by the inference-pool worker to recognize
/// "parent row deleted underfoot" without confusing it with other
/// database errors.
fn is_fk_violation(e: &sqlx::Error) -> bool {
    if let sqlx::Error::Database(db_err) = e {
        return db_err.code().as_deref() == Some("23503");
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_nul_bytes_returns_false_on_clean_input() {
        let mut s = String::from("no nuls here");
        assert!(!strip_nul_bytes(&mut s));
        assert_eq!(s, "no nuls here");
    }

    #[test]
    fn strip_nul_bytes_removes_embedded_nul() {
        let mut s = String::from("before\0after");
        assert!(strip_nul_bytes(&mut s));
        assert_eq!(s, "beforeafter");
    }

    #[test]
    fn strip_nul_bytes_removes_multiple_nuls() {
        let mut s = String::from("\0a\0b\0c\0");
        assert!(strip_nul_bytes(&mut s));
        assert_eq!(s, "abc");
    }

    #[test]
    fn strip_nul_bytes_handles_only_nuls() {
        let mut s = String::from("\0\0\0");
        assert!(strip_nul_bytes(&mut s));
        assert_eq!(s, "");
    }

    #[test]
    fn strip_nul_bytes_preserves_unicode() {
        // Mixed: NUL + multi-byte UTF-8. `retain` operates on `char`s, so
        // the multi-byte sequences must survive untouched.
        let mut s = String::from("héllo\0wörld\0\u{1F600}");
        assert!(strip_nul_bytes(&mut s));
        assert_eq!(s, "héllowörld\u{1F600}");
    }

    #[test]
    fn strip_nul_bytes_preserves_empty_string() {
        let mut s = String::new();
        assert!(!strip_nul_bytes(&mut s));
        assert_eq!(s, "");
    }
}
