//! `EmbeddingBackend` trait — testability seam over the embedding model.
//!
//! Production: `FastembedBackend` wraps `fastembed::TextEmbedding`.
//!
//! Tests: `pgmcp_testing::mocks::DeterministicEmbeddingBackend` implements
//! the trait and returns hash-seeded f32 vectors with no actual model
//! inference. Lets unit tests of tools that exercise the full
//! query → embed → semantic-search path complete in milliseconds without
//! downloading the ~90 MB ONNX model.
//!
//! The pre-existing `EmbedSource` enum stays as the daemon/CLI dispatch
//! shape; its variants now hold an `Arc<dyn EmbeddingBackend>` instead of
//! a `fastembed::TextEmbedding` directly.

use async_trait::async_trait;
use fastembed::TextEmbedding;

use crate::config::EmbeddingsConfig;
use crate::error::{PgmcpError, Result};

/// Compute embedding vectors for input text.
#[async_trait]
pub trait EmbeddingBackend: Send + Sync {
    /// Embed a single text. Returns a vector of length
    /// `EmbeddingsConfig::dimensions` (384 for the default fastembed model).
    async fn embed_one(&self, text: &str) -> Result<Vec<f32>>;

    /// Embed a batch of texts. Default impl loops over `embed_one`; backends
    /// that support real batching (e.g. fastembed's batched ONNX inference)
    /// should override for throughput.
    ///
    /// Reserved for future bulk-embed callers. Not yet invoked by the
    /// daemon's primary embed path (the pool worker batches internally
    /// against the raw model).
    #[allow(dead_code)]
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for t in texts {
            out.push(self.embed_one(t).await?);
        }
        Ok(out)
    }

    /// Human-readable backend label for logging / telemetry.
    #[allow(dead_code)]
    fn name(&self) -> &'static str;
}

/// Production embedding backend backed by `fastembed::TextEmbedding`.
///
/// Inference happens synchronously inside `embed_one` / `embed_batch`. The
/// backend holds the model behind a `tokio::sync::Mutex` so concurrent
/// calls serialize safely. (fastembed's ONNX session is not `Sync`.)
///
/// Currently the daemon's primary embedding path goes through the
/// dedicated worker pool (`EmbedSource::Pool`) for backpressure, and the
/// CLI uses lazy init (`EmbedSource::Lazy`). `FastembedBackend` is the
/// trait-route variant used when callers want to plug a real model into
/// `EmbedSource::Backend(...)` — primarily useful in tests that prefer a
/// real (slow) model over the deterministic mock, or in future production
/// code that bypasses the pool.
#[allow(dead_code)]
pub struct FastembedBackend {
    model: tokio::sync::Mutex<TextEmbedding>,
}

#[allow(dead_code)]
impl FastembedBackend {
    pub fn new(config: &EmbeddingsConfig) -> Result<Self> {
        let model = super::model::create_embedding_model(config)?;
        Ok(Self {
            model: tokio::sync::Mutex::new(model),
        })
    }
}

#[async_trait]
#[allow(dead_code)]
impl EmbeddingBackend for FastembedBackend {
    async fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        let guard = self.model.lock().await;
        let mut vecs = guard
            .embed(vec![text], None)
            .map_err(|e| PgmcpError::Embedding(format!("Embedding failed: {}", e)))?;
        vecs.pop()
            .ok_or_else(|| PgmcpError::Embedding("No embedding returned".into()))
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let guard = self.model.lock().await;
        guard
            .embed(texts.to_vec(), None)
            .map_err(|e| PgmcpError::Embedding(format!("Embedding failed: {}", e)))
    }

    fn name(&self) -> &'static str {
        "fastembed"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn _assert_object_safe(_: Box<dyn EmbeddingBackend>) {}

    #[test]
    fn trait_is_send_sync_and_object_safe() {
        fn _assert<T: Send + Sync>() {}
        _assert::<Arc<dyn EmbeddingBackend>>();
    }
}
