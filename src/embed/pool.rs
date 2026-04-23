//! Dedicated embedding thread pool with batch processing and priority query channel.
//!
//! Each embedding thread owns its own TextEmbedding model instance (no sharing, no locks).
//! Two bounded crossbeam channels provide backpressure:
//! - **query channel** (priority): MCP/API query-time embeddings, drained first
//! - **index channel**: bulk file/commit chunk embeddings

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use crossbeam_channel::{Receiver, Sender, TryRecvError, bounded};
use tracing::{debug, error, info};

use crate::config::EmbeddingsConfig;
use crate::error::{PgmcpError, Result};
use crate::stats::tracker::StatsTracker;

/// Indexing embedding request: either a file chunk or a git commit chunk.
pub enum EmbedIndexRequest {
    /// File chunks with two-phase commit.
    File(EmbedRequest),
    /// Git commit chunks.
    Commit(EmbedCommitRequest),
}

/// A request to embed file chunks and store them.
pub struct EmbedRequest {
    /// File ID in the database.
    pub file_id: i64,
    /// Chunk index, content, start/end lines.
    pub chunks: Vec<ChunkData>,
    /// Database pool for upserting.
    pub db_pool: sqlx::PgPool,
    /// Content hash to finalize after all chunks are inserted (two-phase commit).
    pub content_hash: i64,
}

/// A request to embed git commit chunks and store them.
pub struct EmbedCommitRequest {
    /// Commit ID in the database.
    pub commit_id: i64,
    /// Chunk data to embed.
    pub chunks: Vec<ChunkData>,
    /// Database pool for upserting.
    pub db_pool: sqlx::PgPool,
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

fn embedding_worker(
    id: usize,
    index_rx: Receiver<EmbedIndexRequest>,
    query_rx: Receiver<EmbedQueryRequest>,
    config: &EmbeddingsConfig,
    stats: &StatsTracker,
    shutdown: &AtomicBool,
    rt: &tokio::runtime::Handle,
) {
    // Each worker owns its own model instance
    let model = match super::model::create_embedding_model(config) {
        Ok(m) => m,
        Err(e) => {
            error!(worker_id = id, error = %e, "Failed to create embedding model");
            return;
        }
    };

    debug!(worker_id = id, "Embedding worker started");

    loop {
        if shutdown.load(Ordering::Acquire) {
            break;
        }

        // Priority: drain ALL pending query requests first
        loop {
            match query_rx.try_recv() {
                Ok(req) => process_query_request(&model, req, stats),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        // Then select between query (priority) and index channels, with timeout
        crossbeam_channel::select! {
            recv(query_rx) -> msg => {
                match msg {
                    Ok(req) => process_query_request(&model, req, stats),
                    Err(_) => return,
                }
            }
            recv(index_rx) -> msg => {
                match msg {
                    Ok(req) => {
                        let start = std::time::Instant::now();
                        match req {
                            EmbedIndexRequest::File(file_req) => {
                                process_file_request(&model, file_req, stats, rt, id);
                            }
                            EmbedIndexRequest::Commit(commit_req) => {
                                process_commit_request(&model, commit_req, stats, rt, id);
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

    debug!(worker_id = id, "Embedding worker exiting");
}

fn process_query_request(
    model: &fastembed::TextEmbedding,
    request: EmbedQueryRequest,
    stats: &StatsTracker,
) {
    let result = model
        .embed(vec![&request.text], None)
        .map_err(|e| PgmcpError::Embedding(format!("Embedding failed: {}", e)))
        .and_then(|mut vecs| {
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
    model: &fastembed::TextEmbedding,
    request: EmbedRequest,
    stats: &StatsTracker,
    rt: &tokio::runtime::Handle,
    worker_id: usize,
) {
    let texts: Vec<&str> = request.chunks.iter().map(|c| c.content.as_str()).collect();

    match model.embed(texts, None) {
        Ok(embeddings) => {
            let chunks = request.chunks;
            let db_pool = request.db_pool;
            let file_id = request.file_id;
            let content_hash = request.content_hash;

            rt.block_on(async {
                let mut all_chunks_ok = true;
                for (chunk, embedding) in chunks.iter().zip(embeddings.iter()) {
                    if let Err(e) = crate::db::queries::insert_chunk(
                        &db_pool,
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

                if all_chunks_ok
                    && let Err(e) =
                        crate::db::queries::finalize_file_hash(&db_pool, file_id, content_hash)
                            .await
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
    model: &fastembed::TextEmbedding,
    request: EmbedCommitRequest,
    stats: &StatsTracker,
    rt: &tokio::runtime::Handle,
    worker_id: usize,
) {
    let texts: Vec<&str> = request.chunks.iter().map(|c| c.content.as_str()).collect();

    match model.embed(texts, None) {
        Ok(embeddings) => {
            let chunks = request.chunks;
            let db_pool = request.db_pool;
            let commit_id = request.commit_id;

            rt.block_on(async {
                for (chunk, embedding) in chunks.iter().zip(embeddings.iter()) {
                    if let Err(e) = crate::db::queries::insert_git_commit_chunk(
                        &db_pool,
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
