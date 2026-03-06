//! Dedicated embedding thread pool with batch processing.
//!
//! Each embedding thread owns its own TextEmbedding model instance (no sharing, no locks).
//! A bounded crossbeam channel provides backpressure.

use std::sync::Arc;
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
    pub fn new(config: &EmbeddingsConfig, stats: Arc<StatsTracker>) -> Result<Self> {
        let pool_size = config.pool_size;
        let batch_size = config.batch_size;
        let (tx, rx) = bounded::<EmbedRequest>(batch_size * 2);

        let mut workers = Vec::with_capacity(pool_size);

        for i in 0..pool_size {
            let rx = rx.clone();
            let config = config.clone();
            let stats = Arc::clone(&stats);

            let handle = thread::Builder::new()
                .name(format!("pgmcp-embed-{}", i))
                .spawn(move || {
                    embedding_worker(i, rx, &config, &stats);
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

    /// Shutdown the embedding pool and wait for workers to finish.
    pub fn shutdown(self) {
        drop(self.tx);
        for handle in self.workers {
            let _ = handle.join();
        }
    }
}

fn embedding_worker(
    id: usize,
    rx: Receiver<EmbedRequest>,
    config: &EmbeddingsConfig,
    stats: &StatsTracker,
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

    for request in rx {
        let start = std::time::Instant::now();

        // Batch embed all chunks
        let texts: Vec<&str> = request.chunks.iter().map(|c| c.content.as_str()).collect();

        match model.embed(texts, None) {
            Ok(embeddings) => {
                // Store embeddings in DB
                let rt = tokio::runtime::Handle::try_current();
                if let Ok(rt) = rt {
                    let chunks = request.chunks;
                    let db_pool = request.db_pool;
                    let file_id = request.file_id;

                    rt.block_on(async {
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
                            }
                        }
                    });

                    stats.chunks_embedded.fetch_add(
                        chunks.len() as u64,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                }
            }
            Err(e) => {
                error!(worker_id = id, error = %e, "Embedding batch failed");
            }
        }

        let elapsed = start.elapsed().as_millis() as u64;
        stats
            .embedding_duration_ms
            .fetch_add(elapsed, std::sync::atomic::Ordering::Relaxed);
    }

    debug!(worker_id = id, "Embedding worker exiting");
}
