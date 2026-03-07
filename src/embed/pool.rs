//! Dedicated embedding thread pool with batch processing.
//!
//! Each embedding thread owns its own TextEmbedding model instance (no sharing, no locks).
//! A bounded crossbeam channel provides backpressure.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use crossbeam_channel::{bounded, Sender, Receiver};
use tracing::{debug, error, info};

use crate::config::EmbeddingsConfig;
use crate::error::{PgmcpError, Result};
use crate::stats::tracker::StatsTracker;

/// A request to embed chunks and store them.
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

/// Data for a single chunk to embed.
#[derive(Clone)]
pub struct ChunkData {
    pub chunk_index: i32,
    pub content: String,
    pub start_line: i32,
    pub end_line: i32,
}

/// Dedicated embedding thread pool.
pub struct EmbeddingPool {
    tx: Sender<EmbedRequest>,
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
        let (tx, rx) = bounded::<EmbedRequest>(batch_size * 2);

        // Capture the tokio runtime handle so embedding workers can run async DB queries.
        let rt_handle = tokio::runtime::Handle::current();

        let mut workers = Vec::with_capacity(pool_size);

        for i in 0..pool_size {
            let rx = rx.clone();
            let config = config.clone();
            let stats = Arc::clone(&stats);
            let shutdown = Arc::clone(&shutdown);
            let rt = rt_handle.clone();

            let handle = thread::Builder::new()
                .name(format!("pgmcp-embed-{}", i))
                .spawn(move || {
                    embedding_worker(i, rx, &config, &stats, &shutdown, &rt);
                })
                .map_err(|e| PgmcpError::Other(format!("Failed to spawn embedding worker: {}", e)))?;

            workers.push(handle);
        }

        info!(pool_size, "Embedding pool started");
        Ok(Self { tx, workers })
    }

    /// Get a sender for submitting embedding requests.
    pub fn sender(&self) -> Sender<EmbedRequest> {
        self.tx.clone()
    }

    /// Signal shutdown and return worker handles for joining with custom timeout logic.
    pub fn shutdown_take_handles(self) -> Vec<JoinHandle<()>> {
        drop(self.tx);
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
    rx: Receiver<EmbedRequest>,
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
        // Check shutdown before blocking on recv
        if shutdown.load(Ordering::Acquire) {
            break;
        }

        // Use recv_timeout so we can periodically check the shutdown flag
        let request = match rx.recv_timeout(std::time::Duration::from_millis(500)) {
            Ok(req) => req,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        };

        let start = std::time::Instant::now();

        // Batch embed all chunks
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

                    // Two-phase commit: finalize hash only if all chunks succeeded
                    if all_chunks_ok {
                        if let Err(e) = crate::db::queries::finalize_file_hash(
                            &db_pool, file_id, content_hash,
                        ).await {
                            error!(file_id, error = %e, "Failed to finalize content hash");
                        }
                    }
                });

                stats.chunks_embedded.fetch_add(
                    chunks.len() as u64,
                    Ordering::Relaxed,
                );
            }
            Err(e) => {
                error!(worker_id = id, error = %e, "Embedding batch failed");
            }
        }

        let elapsed = start.elapsed().as_millis() as u64;
        stats
            .embedding_duration_ms
            .fetch_add(elapsed, Ordering::Relaxed);
    }

    debug!(worker_id = id, "Embedding worker exiting");
}
