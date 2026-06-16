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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use arc_swap::ArcSwap;
use crossbeam_channel::{Receiver, Sender, TryRecvError, bounded};
use dashmap::DashMap;
use tracing::{debug, error, info, warn};

use pgvector::SparseVector;

use super::admission;
use super::model::Embedder;
use crate::config::{Config, EmbeddingsConfig, ProjectOverride};
use crate::db::DbClient;
use crate::error::{PgmcpError, Result};
use crate::indexer::{scanner, watcher};
use crate::stats::tracker::StatsTracker;

// `strip_nul_bytes` lives in `crate::indexer::chunker` so every ingestion
// path uses one canonical implementation. The embed-pool worker calls it
// twice (raw content + per-chunk) as defence in depth.
use crate::indexer::chunker::strip_nul_bytes;

mod dedup;
use dedup::*;

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
    /// When `true`, also compute the BGE-M3 learned-sparse vector (Phase 2.3).
    pub want_sparse: bool,
    /// Texts to encode as ColBERT multi-vector matrices (Phase 2.5). The query
    /// plus its rerank candidates ride in one request so they share a single
    /// forward pass / GPU hop. Empty = skip the ColBERT head entirely.
    pub colbert_texts: Vec<String>,
    pub reply: tokio::sync::oneshot::Sender<Result<QueryEmbedResult>>,
}

/// Query-time embedding outputs: always the dense vector, plus the sparse
/// vector when requested and the backbone has a sparse head, plus per-text
/// ColBERT token matrices when `colbert_texts` was non-empty (Phase 2.5).
pub struct QueryEmbedResult {
    pub dense: Vec<f32>,
    pub sparse: Option<SparseVector>,
    /// One entry per `colbert_texts` input, in order. `None` when the backbone
    /// has no ColBERT head; otherwise the L2-normalized per-token vectors.
    pub colbert: Vec<Option<Vec<Vec<f32>>>>,
}

/// Cloneable handle for submitting query-time embedding requests.
/// Routes through the embed pool's priority query channel.
#[derive(Clone)]
pub struct QueryEmbedder {
    tx: Sender<EmbedQueryRequest>,
    /// Shared with the pool so callers can gate on embedder readiness. Reads
    /// `embed_workers_alive` — the count of workers that have finished loading
    /// their model and are in their serve loop.
    stats: Arc<StatsTracker>,
}

impl QueryEmbedder {
    /// True once at least one pool worker has finished loading its model and is
    /// serving — so a query is embedded promptly rather than parked in the
    /// bounded query channel until a worker warms up. Backs the daemon's
    /// serving-readiness gate (`/health` 200; `/api/search` 503-until-ready).
    pub fn is_ready(&self) -> bool {
        self.ready_workers() >= 1
    }

    /// Number of pool workers that have loaded their model and are serving.
    pub fn ready_workers(&self) -> u64 {
        self.stats.embed_workers_alive.load(Ordering::Acquire)
    }

    /// Embed a single query string. Returns the dense embedding vector.
    /// Blocks until a pool worker processes the request.
    pub async fn embed_query(&self, text: String) -> Result<Vec<f32>> {
        Ok(self.embed_query_inner(text, false).await?.dense)
    }

    /// Embed a query for hybrid retrieval: dense vector + (when the backbone
    /// has a sparse head) the BGE-M3 learned-sparse vector. (Phase 2.3)
    pub async fn embed_query_hybrid(&self, text: String) -> Result<QueryEmbedResult> {
        self.embed_query_inner(text, true).await
    }

    async fn embed_query_inner(&self, text: String, want_sparse: bool) -> Result<QueryEmbedResult> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(EmbedQueryRequest {
                text,
                want_sparse,
                colbert_texts: Vec::new(),
                reply: reply_tx,
            })
            .map_err(|_| PgmcpError::Embedding("Embed pool shut down".into()))?;
        reply_rx
            .await
            .map_err(|_| PgmcpError::Embedding("Embed worker dropped reply".into()))?
    }

    /// Encode a batch of texts as ColBERT multi-vector matrices (Phase 2.5).
    /// Used by the API rerank stage to score query↔candidate MaxSim. The dense
    /// vector of the request is computed-but-ignored; `colbert_texts` carries
    /// the real payload so the whole batch shares one forward pass. Returns one
    /// entry per input (in order); `None` entries mean the backbone has no
    /// ColBERT head (caller should fall back to the prior ordering).
    pub async fn embed_colbert_batch(
        &self,
        texts: Vec<String>,
    ) -> Result<Vec<Option<Vec<Vec<f32>>>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(EmbedQueryRequest {
                // A non-empty dense text keeps the worker's dense path well-defined;
                // the result's `dense`/`sparse` fields are unused by the caller.
                text: texts[0].clone(),
                want_sparse: false,
                colbert_texts: texts,
                reply: reply_tx,
            })
            .map_err(|_| PgmcpError::Embedding("Embed pool shut down".into()))?;
        let result = reply_rx
            .await
            .map_err(|_| PgmcpError::Embedding("Embed worker dropped reply".into()))??;
        Ok(result.colbert)
    }
}

/// Dedicated embedding thread pool with dual channels (query priority + index).
///
/// Resilience (the module's history with "sending on a disconnected channel"):
/// the pool's monitor thread retains the original `index_rx`/`query_rx`
/// receivers, so the channels can never reach `Disconnected` while the pool is
/// alive — even if every worker thread dies, scanner/watcher sends keep
/// buffering instead of erroring. The monitor watches the worker handles and
/// respawns any slot that exits unexpectedly (e.g. construction-budget
/// exhaustion under sustained VRAM pressure), so a dead worker recovers
/// without a daemon restart.
pub struct EmbeddingPool {
    index_tx: Sender<EmbedIndexRequest>,
    query_tx: Sender<EmbedQueryRequest>,
    /// Worker handles, shared with the liveness monitor (it detects a dead
    /// slot via `JoinHandle::is_finished` and respawns it). A slot is briefly
    /// `None` while a respawn is in flight.
    workers: Arc<Mutex<Vec<Option<JoinHandle<()>>>>>,
    /// Liveness monitor handle (joined on shutdown).
    monitor: Option<JoinHandle<()>>,
    /// Shutdown flag shared with workers + monitor.
    shutdown: Arc<AtomicBool>,
    /// Stats handle, retained so `query_embedder()` can hand callers a
    /// readiness view via `embed_workers_alive`.
    stats: Arc<StatsTracker>,
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

        let workers: Arc<Mutex<Vec<Option<JoinHandle<()>>>>> =
            Arc::new(Mutex::new(Vec::with_capacity(pool_size)));
        {
            let mut slots = workers.lock().expect("embed worker registry poisoned");
            for i in 0..pool_size {
                let handle = spawn_worker(
                    i,
                    index_rx.clone(),
                    query_rx.clone(),
                    config.clone(),
                    Arc::clone(&stats),
                    Arc::clone(&shutdown),
                    rt_handle.clone(),
                )?;
                slots.push(Some(handle));
            }
        }

        // Liveness monitor. It is also the long-lived owner of an `index_rx` /
        // `query_rx` clone — which is what keeps the channels from ever
        // reaching `Disconnected` while the pool exists, so a fully-dead pool
        // no longer floods the scanner with "sending on a disconnected
        // channel".
        let monitor = spawn_monitor(
            index_rx,
            query_rx,
            config.clone(),
            Arc::clone(&stats),
            Arc::clone(&shutdown),
            rt_handle,
            Arc::clone(&workers),
        )?;

        info!(pool_size, "Embedding pool started");
        Ok(Self {
            index_tx,
            query_tx,
            workers,
            monitor: Some(monitor),
            shutdown,
            stats,
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
            stats: Arc::clone(&self.stats),
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
        // Signal shutdown (idempotent — the daemon usually set it already) so
        // the monitor stops respawning and workers exit their loops.
        self.shutdown.store(true, Ordering::Release);
        drop(self.index_tx);
        drop(self.query_tx);
        // Join the monitor first so it cannot respawn a worker we are about to
        // reap. Its loop is shutdown-aware (wakes within ~100ms), so this is
        // bounded.
        if let Some(monitor) = self.monitor {
            let _ = monitor.join();
        }
        // The registry is now quiescent; hand the worker handles to the caller
        // (the daemon joins them with its own per-handle timeout policy).
        let mut handles = Vec::new();
        if let Ok(mut slots) = self.workers.lock() {
            for slot in slots.iter_mut() {
                if let Some(h) = slot.take() {
                    handles.push(h);
                }
            }
        }
        handles
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

/// Bound on event-loop re-entries before a worker gives up and lets the
/// monitor respawn it. Re-entry only happens on an unexpected (non-shutdown)
/// event-loop exit, which should never occur now that the pool retains the
/// senders — so this is purely a spin guard.
const MAX_EVENT_LOOP_REENTRIES: u32 = 100;

/// How often the liveness monitor polls worker handles.
const MONITOR_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Minimum delay between respawns of the SAME worker slot, so a slot that
/// keeps dying (e.g. a persistent driver fault) can't busy-spawn.
const RESPAWN_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(30);

/// Spawn one embedding worker thread for slot `id`.
#[allow(clippy::too_many_arguments)]
fn spawn_worker(
    id: usize,
    index_rx: Receiver<EmbedIndexRequest>,
    query_rx: Receiver<EmbedQueryRequest>,
    config: EmbeddingsConfig,
    stats: Arc<StatsTracker>,
    shutdown: Arc<AtomicBool>,
    rt: tokio::runtime::Handle,
) -> Result<JoinHandle<()>> {
    thread::Builder::new()
        .name(format!("pgmcp-embed-{}", id))
        .spawn(move || {
            embedding_worker(id, index_rx, query_rx, &config, &stats, &shutdown, &rt);
        })
        .map_err(|e| PgmcpError::Other(format!("Failed to spawn embedding worker: {}", e)))
}

/// Liveness monitor: respawns any worker slot whose thread has exited while the
/// pool is still running. With the per-task `catch_unwind` guard and the
/// supervisor's re-entry, a worker should only exit on shutdown or after
/// exhausting its construction-retry budget; this recovers the latter case
/// (once VRAM/driver pressure clears) without a daemon restart. Holding the
/// `index_rx`/`query_rx` clones for the monitor's lifetime also keeps the
/// channels alive (never `Disconnected`) regardless of worker deaths.
#[allow(clippy::too_many_arguments)]
fn spawn_monitor(
    index_rx: Receiver<EmbedIndexRequest>,
    query_rx: Receiver<EmbedQueryRequest>,
    config: EmbeddingsConfig,
    stats: Arc<StatsTracker>,
    shutdown: Arc<AtomicBool>,
    rt: tokio::runtime::Handle,
    workers: Arc<Mutex<Vec<Option<JoinHandle<()>>>>>,
) -> Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("pgmcp-embed-monitor".into())
        .spawn(move || {
            let pool_size = workers.lock().map(|w| w.len()).unwrap_or(0);
            // `None` = never respawned this slot → an immediate first respawn
            // is allowed. Avoids `Instant - Duration` underflow at startup.
            let mut last_respawn: Vec<Option<Instant>> = vec![None; pool_size];
            while !shutdown.load(Ordering::Acquire) {
                sleep_with_shutdown(MONITOR_POLL_INTERVAL, &shutdown);
                if shutdown.load(Ordering::Acquire) {
                    break;
                }
                let mut slots = match workers.lock() {
                    Ok(s) => s,
                    Err(_) => return,
                };
                for id in 0..slots.len() {
                    let dead = slots[id].as_ref().map(|h| h.is_finished()).unwrap_or(true);
                    if !dead {
                        continue;
                    }
                    if let Some(t) = last_respawn[id]
                        && t.elapsed() < RESPAWN_COOLDOWN
                    {
                        continue;
                    }
                    // Re-check shutdown so we don't respawn during teardown.
                    if shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    if let Some(dead_handle) = slots[id].take() {
                        let _ = dead_handle.join();
                    }
                    error!(
                        worker_id = id,
                        "embedding worker slot exited unexpectedly; respawning"
                    );
                    match spawn_worker(
                        id,
                        index_rx.clone(),
                        query_rx.clone(),
                        config.clone(),
                        Arc::clone(&stats),
                        Arc::clone(&shutdown),
                        rt.clone(),
                    ) {
                        Ok(h) => {
                            slots[id] = Some(h);
                            stats.embed_worker_restarts.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => {
                            error!(worker_id = id, error = %e, "embedding worker respawn failed; will retry");
                        }
                    }
                    last_respawn[id] = Some(Instant::now());
                }
            }
        })
        .map_err(|e| PgmcpError::Other(format!("Failed to spawn embed monitor: {}", e)))
}

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
    if shutdown.load(Ordering::Acquire) {
        return;
    }

    // GPU admission: hold one resident-copy permit for this worker's entire
    // lifetime so the pool workers + migration cron together never exceed the
    // VRAM budget (`embeddings.gpu_max_resident_embedders`). Released on return
    // — including unwind — so a monitor respawn re-acquires cleanly. `None`
    // when GPU admission is disabled (CPU mode / `use_gpu = false`).
    let _gpu_permit = match admission::semaphore() {
        Some(_) => match admission::acquire_owned(rt) {
            Some(permit) => Some(permit),
            // Semaphore closed (teardown) — nothing to do.
            None => return,
        },
        None => None,
    };

    let mut consecutive_failures: u32 = 0;
    let mut backoff = std::time::Duration::from_secs(1);
    let mut reentries: u32 = 0;
    // Phase C.10: time-to-first-model-ready (embed pool warmup).
    // The recovery-times harness greps `phase="ready"` from the
    // structured log to derive the embed-pool warmup row.
    let worker_start = std::time::Instant::now();

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
                consecutive_failures = 0;
                let elapsed = worker_start.elapsed().as_secs_f64();
                info!(
                    target: "pgmcp::recovery_times",
                    worker_id = id,
                    phase = "ready",
                    elapsed = elapsed,
                    "embed_pool_warmup"
                );
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

        if shutdown.load(Ordering::Acquire) {
            debug!(worker_id = id, "Embedding worker exiting (shutdown)");
            return;
        }

        // The event loop returned without a shutdown signal. Because the pool
        // retains the channel senders this should not happen; treat it as an
        // unexpected disconnect and rebuild the model + re-enter rather than
        // letting the slot die. Bounded — a genuinely wedged pool surfaces via
        // the monitor's respawn path instead of spinning here. `model` drops at
        // the end of this iteration, freeing its GPU memory before the rebuild.
        reentries += 1;
        stats
            .embed_event_loop_reentries
            .fetch_add(1, Ordering::Relaxed);
        if reentries > MAX_EVENT_LOOP_REENTRIES {
            error!(
                worker_id = id,
                "embedding worker exceeded event-loop re-entry budget; exiting (monitor will respawn)"
            );
            return;
        }
        warn!(
            worker_id = id,
            reentries, "embedding event loop exited without shutdown; rebuilding model"
        );
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

/// Run a per-task closure under `catch_unwind` so a panicking task (an
/// unexpected `.expect()`, a slice panic deep in chunking, or a panic in a
/// `block_on`-driven DB future) is logged and counted instead of unwinding the
/// worker thread — which would drop the worker's channel receiver and, before
/// the pool retained its senders, could permanently disconnect the index
/// channel. The worker keeps its loaded model and live receiver and moves on
/// to the next task.
///
/// `AssertUnwindSafe` is sound here: the captured `&Embedder` is used read-only
/// (forward passes don't mutate it), `&StatsTracker` is all atomics, and the
/// runtime `Handle` is unwind-safe — no torn invariant is observable after a
/// caught panic.
fn run_task_caught(id: usize, stats: &StatsTracker, kind: &str, task: impl FnOnce()) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(task)).is_err() {
        error!(
            worker_id = id,
            kind, "embedding task panicked; worker recovered"
        );
        stats.embed_task_panics.fetch_add(1, Ordering::Relaxed);
    }
}

/// Handle one `query_rx` select result. Returns `false` when the channel has
/// disconnected (the worker should return). Extracted so the intake-gated and
/// ungated `select!` branches share one query path with no drift.
fn handle_query_select(
    id: usize,
    model: &Embedder,
    stats: &StatsTracker,
    msg: std::result::Result<EmbedQueryRequest, crossbeam_channel::RecvError>,
) -> bool {
    match msg {
        Ok(req) => {
            run_task_caught(id, stats, "query", || {
                process_query_request(model, req, stats)
            });
            true
        }
        Err(_) => false,
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
                Ok(req) => {
                    run_task_caught(id, stats, "query", || {
                        process_query_request(model, req, stats)
                    });
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        // Intake gate (src/health): when the DB is down or disk is under
        // pressure, do NOT pull the next index task — leave it buffered on the
        // bounded channel so the scanner/watcher backpressures and no indexing
        // work is pulled-and-dropped. Queries (read-only) keep flowing either
        // way. The file already in hand (if the DB drops mid-task) rides the
        // existing retry helpers and, on ultimate failure, is re-picked-up by
        // the mtime `rescan_workspace` reconciliation — see docs ADR-015.
        let intake_open = stats.db_health().is_up() && !stats.disk_pressure().is_paused();
        if intake_open {
            crossbeam_channel::select! {
                recv(query_rx) -> msg => {
                    if !handle_query_select(id, model, stats, msg) {
                        return;
                    }
                }
                recv(index_rx) -> msg => {
                    match msg {
                        Ok(req) => {
                            let start = std::time::Instant::now();
                            run_task_caught(id, stats, "index", || match req {
                                EmbedIndexRequest::IndexFile(task) => {
                                    process_index_file_task(model, task, stats, rt, id);
                                }
                                EmbedIndexRequest::File(file_req) => {
                                    process_file_request(model, file_req, stats, rt, id);
                                }
                                EmbedIndexRequest::Commit(commit_req) => {
                                    process_commit_request(model, commit_req, stats, rt, id);
                                }
                            });
                            let elapsed = start.elapsed().as_millis() as u64;
                            stats.embedding_duration_ms.fetch_add(elapsed, Ordering::Relaxed);
                        }
                        Err(_) => return,
                    }
                }
                default(std::time::Duration::from_millis(500)) => {}
            }
        } else {
            // Gate closed: block only on queries, re-polling the gate every
            // 500 ms. Index work stays queued (backpressure), not lost.
            crossbeam_channel::select! {
                recv(query_rx) -> msg => {
                    if !handle_query_select(id, model, stats, msg) {
                        return;
                    }
                }
                default(std::time::Duration::from_millis(500)) => {}
            }
        }
    }
}

fn process_query_request(model: &Embedder, request: EmbedQueryRequest, stats: &StatsTracker) {
    let result = (|| {
        let dense = model
            .embed(&[&request.text])?
            .pop()
            .ok_or_else(|| PgmcpError::Embedding("No embedding returned".into()))?;
        let sparse = if request.want_sparse {
            model
                .embed_sparse(&[&request.text])?
                .into_iter()
                .next()
                .flatten()
        } else {
            None
        };
        let colbert = if request.colbert_texts.is_empty() {
            Vec::new()
        } else {
            let refs: Vec<&str> = request.colbert_texts.iter().map(|s| s.as_str()).collect();
            model.embed_colbert(&refs)?
        };
        Ok(QueryEmbedResult {
            dense,
            sparse,
            colbert,
        })
    })();

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

    use crate::embed::failure_kind::FailureKind;
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
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Race: the path was queued by the scanner but deleted before
            // this worker reached it. Common when worktrees are torn down
            // mid-rescan or when an editor's temp file vanishes. Demote
            // the noise AND clean up the orphan `indexed_files` row so
            // the next scan doesn't requeue it. `delete_file` cascades
            // through `file_chunks` and other dependent tables.
            debug!(
                path = %path_str,
                worker_id,
                "fs::metadata: file disappeared (concurrent delete)"
            );
            let _ = rt.block_on(db.delete_file(&path_str));
            stats.files_failed.fetch_add(1, Ordering::Relaxed);
            return;
        }
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
                let id = upsert_project_with_retry(
                    db.as_ref(),
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
                let id = upsert_project_with_retry(
                    db.as_ref(),
                    &workspace,
                    &workspace,
                    "default",
                    None,
                    None,
                )
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

            let empty_chunks: &[crate::db::queries::ChunkInsert<'_>] = &[];
            replace_indexed_file_with_retry(
                db.as_ref(),
                &path_str,
                worker_id,
                crate::db::queries::IndexedFileReplacement {
                    project_id,
                    path: &path_str,
                    relative_path: &relative,
                    language: &language,
                    size_bytes,
                    content: None,
                    content_hash,
                    line_count: 0,
                    truncated: true,
                    content_recoverable_from_disk: false,
                    modified_at,
                    chunks: empty_chunks,
                },
            )
            .await?;
            Ok(false)
        });
        match res {
            Ok(true) => {
                // Oversize file unchanged (size+mtime hash matched) — confirm
                // verified so it doesn't read as falsely stale.
                let _ = rt.block_on(db.mark_file_verified(&path_str));
            }
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
                let _ = rt.block_on(db.record_index_failure(
                    &path_str,
                    FailureKind::DocExtractFailed,
                    "document extractor returned None",
                ));
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
                // F4: per-file extraction timeout is a content/tool
                // interaction issue, not a service-health failure. The
                // `documents_extraction_timeout` counter still increments
                // (visible via the metrics endpoint). Demote to warn! so
                // operators tailing the log can spot real service errors.
                // See plan ~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md
                // F4.
                warn!(
                    path = %path_str,
                    worker_id,
                    lang = %language,
                    "Document extraction timed out"
                );
                stats
                    .documents_extraction_timeout
                    .fetch_add(1, Ordering::Relaxed);
                // Ledger for bounded retry: a doc that times out will time out
                // again on every reconcile until it changes. (Tracked by its own
                // counter above, so `files_failed` is intentionally not bumped.)
                let _ = rt.block_on(db.record_index_failure(
                    &path_str,
                    FailureKind::DocExtractTimeout,
                    "document extraction timed out",
                ));
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
                let _ = rt.block_on(db.record_index_failure(
                    &path_str,
                    FailureKind::DocExtractOom,
                    "document extraction subprocess killed (rlimit/OOM)",
                ));
                return;
            }
            Err(e) => {
                // F4: per-file extraction failure (pandoc / pdftotext /
                // ps2ascii rejecting the file's content) is a content
                // problem, not a service-health failure. The
                // `files_failed` counter still increments. Demote to
                // warn! to match the timeout case above.
                warn!(
                    path = %path_str,
                    worker_id,
                    lang = %language,
                    error = %e,
                    "Document extraction failed"
                );
                stats.files_failed.fetch_add(1, Ordering::Relaxed);
                let _ = rt.block_on(db.record_index_failure(
                    &path_str,
                    FailureKind::DocExtractFailed,
                    &e.to_string(),
                ));
                return;
            }
        }
    } else {
        match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                // UTF-8 validation failure isn't a system error — it's a
                // content-shape mismatch (the file's extension lied; common
                // for `__MACOSX/._*` AppleDouble forks, binary `.java`/`.py`
                // fixtures, etc.). Demote so it doesn't drown real I/O
                // failures in the log. The file is still skipped.
                if e.kind() == std::io::ErrorKind::InvalidData {
                    warn!(path = %path_str, worker_id, error = %e, "fs::read_to_string: not valid UTF-8 (skipping)");
                    // Content-intrinsic: ledger for bounded retry so the scanner
                    // stops re-reading a mislabeled binary on every reconcile.
                    let _ = rt.block_on(db.record_index_failure(
                        &path_str,
                        FailureKind::NotUtf8,
                        &e.to_string(),
                    ));
                } else {
                    // Transient I/O — not ledgered (self-heals on the next pass).
                    error!(path = %path_str, worker_id, error = %e, "fs::read_to_string failed");
                }
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
        Ok(DedupAction::Level2Skip) => {
            // Content unchanged at this path (e.g. a git-touch that bumped mtime
            // without changing bytes) — stamp verified so `file_info`/`orient`
            // stop reporting it as stale even though `indexed_at` is unchanged.
            let _ = rt.block_on(db.mark_file_verified(&path_str));
            return;
        }
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

    // Level-2 content-hash skip. The DB mutation happens only after chunks
    // and embeddings are ready, inside one replacement transaction.
    let skip_res = rt.block_on(async {
        if let Ok(Some(existing)) = db.get_content_hash(&path_str).await
            && existing == content_hash
        {
            return Ok::<bool, sqlx::Error>(true);
        }
        Ok(false)
    });
    match skip_res {
        Ok(true) => {
            // Content unchanged — stamp verified (the false-staleness fix).
            let _ = rt.block_on(db.mark_file_verified(&path_str));
            return;
        }
        Ok(false) => {}
        Err(e) => {
            error!(path = %path_str, worker_id, error = %e, "content-hash check failed");
            stats.files_failed.fetch_add(1, Ordering::Relaxed);
            return;
        }
    }
    let line_count = content.lines().count() as i32;

    // Chunk content with the language-appropriate chunker. JSONL gets
    // record-aware variants for Claude/Codex session transcripts; latex,
    // org, rst, and markdown get heading-aware chunking (content with no
    // headings falls back to paragraph mode internally); pdf/postscript/
    // docx/etc. use paragraph-aware chunking; everything else uses the
    // line chunker.
    let mut chunks = if &*language == "jsonl" && claude_chunker::is_claude_session_transcript(&path)
    {
        claude_chunker::chunk_claude_jsonl(&content)
    } else if &*language == "jsonl" && codex_chunker::is_codex_jsonl(&path) {
        codex_chunker::chunk_codex_jsonl(&content)
    } else if &*language == "jsonl" {
        chunker::chunk_jsonl_content(&content)
    } else if matches!(&*language, "latex" | "org" | "rst" | "markdown") {
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
    // strings and can introduce raw `\0` from `"\u0000"` escapes in tool
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
        let empty_chunks: &[crate::db::queries::ChunkInsert<'_>] = &[];
        if let Err(e) = rt.block_on(replace_indexed_file_with_retry(
            db.as_ref(),
            &path_str,
            worker_id,
            crate::db::queries::IndexedFileReplacement {
                project_id,
                path: &path_str,
                relative_path: &relative_path,
                language: &language,
                size_bytes,
                content: stored_content,
                content_hash,
                line_count,
                truncated: false,
                content_recoverable_from_disk,
                modified_at,
                chunks: empty_chunks,
            },
        )) {
            error!(path = %path_str, worker_id, error = %e,
                   "replace_indexed_file failed (empty chunks)");
            stats.files_failed.fetch_add(1, Ordering::Relaxed);
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

    // Replace metadata + chunks + finalized hash in one transaction. Three
    // outcomes per file:
    //   - all_ok: every chunk inserted and the final hash committed.
    //   - aborted_fk: parent row deleted underfoot (PG SQLSTATE 23503);
    //     log once at warn!, increment files_aborted_fk.
    //   - other_err: any non-FK error during replacement. The transaction
    //     rolls back, preserving the previous complete indexed state.
    //
    // Wall-clock duration of the replacement transaction is accumulated in
    // `pool_pressure_ms_total` so operators can spot DB contention in the
    // active indexing path.
    enum InsertOutcome {
        AllOk,
        AbortedFk,
        OtherErr,
    }
    let total_chunks = chunks.len();
    let chunk_inserts: Vec<crate::db::queries::ChunkInsert<'_>> = chunks
        .iter()
        .zip(embeddings.iter())
        .map(|(chunk, embedding)| crate::db::queries::ChunkInsert {
            chunk_index: chunk.chunk_index,
            content: chunk.content.as_str(),
            start_line: chunk.start_line,
            end_line: chunk.end_line,
            embedding: embedding.as_slice(),
        })
        .collect();
    let batch_started = std::time::Instant::now();
    let outcome = rt.block_on(async {
        match replace_indexed_file_with_retry(
            db.as_ref(),
            &path_str,
            worker_id,
            crate::db::queries::IndexedFileReplacement {
                project_id,
                path: &path_str,
                relative_path: &relative_path,
                language: &language,
                size_bytes,
                content: stored_content,
                content_hash,
                line_count,
                truncated: false,
                content_recoverable_from_disk,
                modified_at,
                chunks: &chunk_inserts,
            },
        )
        .await
        {
            Ok(_file_id) => InsertOutcome::AllOk,
            Err(e) => {
                if is_fk_violation(&e) {
                    tracing::warn!(
                        path = %path_str,
                        chunks_total = total_chunks,
                        worker_id,
                        reason = "parent row deleted underfoot — likely \
                                  pgmcp reindex --force or external admin SQL",
                        "replace_indexed_file aborted (FK violation)"
                    );
                    InsertOutcome::AbortedFk
                } else {
                    error!(
                        path = %path_str,
                        chunks_total = total_chunks,
                        worker_id,
                        error = %e,
                        "replace_indexed_file failed; transaction rolled back"
                    );
                    InsertOutcome::OtherErr
                }
            }
        }
    });
    let batch_elapsed_ms = batch_started.elapsed().as_millis() as u64;
    stats
        .pool_pressure_ms_total
        .fetch_add(batch_elapsed_ms, Ordering::Relaxed);
    stats
        .embed_chunk_batches_total
        .fetch_add(1, Ordering::Relaxed);

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

/// `true` iff the sqlx error is PostgreSQL `lock_not_available`
/// (SQLSTATE `55P03`), which is how server-side `lock_timeout` reports
/// transient lock contention.
fn is_lock_timeout(e: &sqlx::Error) -> bool {
    if let sqlx::Error::Database(db_err) = e {
        return db_err.code().as_deref() == Some("55P03");
    }
    false
}

/// Retry the active indexer's all-or-nothing file replacement on transient
/// lock contention. The caller rebuilds embeddings before this point, so a
/// retry only replays the short DB transaction, not model inference.
async fn replace_indexed_file_with_retry(
    db: &dyn DbClient,
    path: &str,
    worker_id: usize,
    replacement: crate::db::queries::IndexedFileReplacement<'_>,
) -> std::result::Result<i64, sqlx::Error> {
    const BACKOFFS_MS: &[u64] = &[500, 1_500, 3_000];

    let mut last_err: Option<sqlx::Error> = None;
    let schedule = std::iter::once(None).chain(BACKOFFS_MS.iter().copied().map(Some));
    for (attempt, backoff_ms) in schedule.enumerate() {
        if let Some(ms) = backoff_ms {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            if let Some(ref e) = last_err {
                warn!(
                    path,
                    worker_id,
                    attempt,
                    backoff_ms = ms,
                    error = %e,
                    "replace_indexed_file: retrying after lock_timeout"
                );
            }
        }

        match db.replace_indexed_file(replacement.clone()).await {
            Ok(file_id) => return Ok(file_id),
            Err(e) if is_lock_timeout(&e) => {
                last_err = Some(e);
                continue;
            }
            Err(e) => return Err(e),
        }
    }

    Err(last_err.unwrap_or(sqlx::Error::PoolTimedOut))
}

/// Retry `db.upsert_project` on `sqlx::Error::PoolTimedOut`. Bounded
/// exponential backoff (1s, 2s, 4s; ~7s total worst-case sleep) so a
/// short cron-driven contention burst doesn't cause the indexer to
/// silently drop files. Non-pool errors bubble immediately so genuine
/// problems aren't masked by retries.
///
/// Why this lives in the embed pool instead of the DbClient impl: the
/// retry semantics are caller-specific — short bursty contention from
/// the embed pool is fine to absorb here, but synchronous CLI commands
/// that wrap `upsert_project` (e.g. `pgmcp init-project`) want the
/// error to surface immediately rather than block for ~7s.
async fn upsert_project_with_retry(
    db: &dyn DbClient,
    workspace_path: &str,
    project_path: &str,
    name: &str,
    git_common_dir: Option<&str>,
    git_root_commits: Option<&str>,
) -> std::result::Result<i32, sqlx::Error> {
    retry_pool_timeout(&[1_000, 2_000, 4_000], workspace_path, name, || async {
        db.upsert_project(
            workspace_path,
            project_path,
            name,
            git_common_dir,
            git_root_commits,
        )
        .await
    })
    .await
}

/// Generic exponential-backoff retry over a fallible async operation.
/// Retries while the result is `Err(sqlx::Error::PoolTimedOut)`; bubbles
/// any other error immediately. The `backoffs_ms` slice defines the
/// sleeps between successive retries — its length sets the retry budget
/// (`len() + 1` total attempts). Empty slice ⇒ single attempt, no retry.
///
/// Extracted from `upsert_project_with_retry` so it can be exercised by
/// a unit test without standing up a full `DbClient` mock.
async fn retry_pool_timeout<F, Fut, T>(
    backoffs_ms: &[u64],
    workspace_path: &str,
    name: &str,
    mut op: F,
) -> std::result::Result<T, sqlx::Error>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::result::Result<T, sqlx::Error>>,
{
    let mut last_err: Option<sqlx::Error> = None;
    let schedule = std::iter::once(None).chain(backoffs_ms.iter().copied().map(Some));
    for (attempt, backoff_ms) in schedule.enumerate() {
        if let Some(ms) = backoff_ms {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            if let Some(ref e) = last_err {
                warn!(
                    workspace_path,
                    name,
                    attempt,
                    backoff_ms = ms,
                    error = %e,
                    "upsert_project: retrying after PoolTimedOut"
                );
            }
        }
        match op().await {
            Ok(v) => return Ok(v),
            Err(sqlx::Error::PoolTimedOut) => {
                last_err = Some(sqlx::Error::PoolTimedOut);
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap_or(sqlx::Error::PoolTimedOut))
}

#[cfg(test)]
mod tests {
    use super::*;

    // `strip_nul_bytes` is tested in `src/indexer/chunker.rs` where it now
    // lives. The embed-pool worker still depends on it (two defence-in-
    // depth call sites at lines 890 and 1102), but the canonical
    // implementation and tests are co-located with the rest of the
    // chunking layer.

    // --- retry_pool_timeout ------------------------------------------------
    //
    // We pause tokio time so the 1s/2s/4s sleeps don't actually slow down
    // the test suite. `tokio::test(start_paused = true)` + `advance` lets
    // us assert on the retry schedule deterministically.

    use std::cell::Cell;

    // --- run_task_caught: per-task panic isolation (F2 P1-a) ---------------
    #[test]
    fn run_task_caught_isolates_panics() {
        let stats = StatsTracker::new();

        // A normal task runs and is not counted as a panic.
        let mut ran = false;
        run_task_caught(0, &stats, "test", || ran = true);
        assert!(ran, "task closure should run");
        assert_eq!(stats.embed_task_panics.load(Ordering::Relaxed), 0);

        // A panicking task is caught (does NOT unwind the caller) and counted,
        // so a real worker keeps its model + channel receiver and moves to the
        // next task instead of dying and permanently disconnecting the channel.
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {})); // silence the intentional panic
        run_task_caught(7, &stats, "test", || panic!("boom"));
        std::panic::set_hook(prev);
        assert_eq!(stats.embed_task_panics.load(Ordering::Relaxed), 1);

        // The caller survived the panic and can run another task.
        run_task_caught(0, &stats, "test", || { /* ok */ });
        assert_eq!(stats.embed_task_panics.load(Ordering::Relaxed), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_returns_immediately_on_success() {
        let calls = Cell::new(0u32);
        let result: std::result::Result<i32, sqlx::Error> =
            retry_pool_timeout(&[1_000, 2_000, 4_000], "/ws", "p", || {
                calls.set(calls.get() + 1);
                async { Ok(42) }
            })
            .await;
        assert_eq!(result.expect("ok"), 42);
        assert_eq!(calls.get(), 1, "no retries on success");
    }

    #[tokio::test(start_paused = true)]
    async fn retry_recovers_after_transient_pool_timeout() {
        let calls = Cell::new(0u32);
        let result: std::result::Result<i32, sqlx::Error> =
            retry_pool_timeout(&[1_000, 2_000, 4_000], "/ws", "p", || {
                let n = calls.get() + 1;
                calls.set(n);
                async move {
                    if n < 3 {
                        Err(sqlx::Error::PoolTimedOut)
                    } else {
                        Ok(7)
                    }
                }
            })
            .await;
        assert_eq!(result.expect("ok"), 7);
        assert_eq!(calls.get(), 3, "succeeded on 3rd attempt after 2 retries");
    }

    #[tokio::test(start_paused = true)]
    async fn retry_exhausts_budget_then_returns_pool_timed_out() {
        // 3 backoffs ⇒ 4 attempts total; if all PoolTimedOut, surface the
        // last seen error (PoolTimedOut). Without this exhaustion path the
        // helper would either loop forever or silently swallow the error.
        let calls = Cell::new(0u32);
        let result: std::result::Result<i32, sqlx::Error> =
            retry_pool_timeout(&[1_000, 2_000, 4_000], "/ws", "p", || {
                calls.set(calls.get() + 1);
                async { Err(sqlx::Error::PoolTimedOut) }
            })
            .await;
        assert!(matches!(result, Err(sqlx::Error::PoolTimedOut)));
        assert_eq!(calls.get(), 4, "1 initial + 3 retries = 4 attempts");
    }

    #[tokio::test(start_paused = true)]
    async fn retry_bubbles_non_pool_errors_immediately() {
        // The whole point of retrying *only* PoolTimedOut is that other
        // errors (FK violation, constraint check, syntax error, etc.)
        // would not be fixed by waiting — they'd just waste 7 seconds.
        let calls = Cell::new(0u32);
        let result: std::result::Result<i32, sqlx::Error> =
            retry_pool_timeout(&[1_000, 2_000, 4_000], "/ws", "p", || {
                calls.set(calls.get() + 1);
                async { Err(sqlx::Error::RowNotFound) }
            })
            .await;
        assert!(matches!(result, Err(sqlx::Error::RowNotFound)));
        assert_eq!(calls.get(), 1, "non-PoolTimedOut bubbles on first attempt");
    }

    #[tokio::test(start_paused = true)]
    async fn retry_empty_backoff_schedule_is_single_attempt() {
        // Edge case: backoffs_ms = &[] degrades to a single try, no retries.
        // Useful for callers that want the helper's error-type discrimination
        // without the wait-and-retry behavior.
        let calls = Cell::new(0u32);
        let result: std::result::Result<i32, sqlx::Error> =
            retry_pool_timeout(&[], "/ws", "p", || {
                calls.set(calls.get() + 1);
                async { Err(sqlx::Error::PoolTimedOut) }
            })
            .await;
        assert!(matches!(result, Err(sqlx::Error::PoolTimedOut)));
        assert_eq!(calls.get(), 1);
    }
}
