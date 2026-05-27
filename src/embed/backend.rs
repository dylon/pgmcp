//! `EmbeddingBackend` trait — testability seam over the embedding model.
//!
//! Production: `CandleBackend` wraps the candle-based `Embedder`.
//!
//! Tests: `pgmcp_testing::mocks::DeterministicEmbeddingBackend` implements
//! the trait and returns hash-seeded f32 vectors with no actual model
//! inference. Lets unit tests of tools that exercise the full
//! query → embed → semantic-search path complete in milliseconds without
//! downloading the BERT model.
//!
//! The pre-existing `EmbedSource` enum stays as the daemon/CLI dispatch
//! shape; its variants now hold an `Arc<dyn EmbeddingBackend>` instead of
//! a concrete model directly.

use async_trait::async_trait;

use super::model::Embedder;
use crate::config::EmbeddingsConfig;
use crate::error::{PgmcpError, Result};

/// Compute embedding vectors for input text.
#[async_trait]
pub trait EmbeddingBackend: Send + Sync {
    /// Embed a single text. Returns a vector of length
    /// `EmbeddingsConfig::dimensions` (1024 for BGE-M3).
    async fn embed_one(&self, text: &str) -> Result<Vec<f32>>;

    /// Embed a batch of texts. Default impl loops over `embed_one`; backends
    /// that support real batching should override for throughput.
    ///
    /// Reserved for future bulk-embed callers.
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

/// Production embedding backend backed by the candle `Embedder`.
///
/// Inference happens synchronously inside `embed_one` / `embed_batch`. The
/// backend holds the model behind a `parking_lot::Mutex` rather than a
/// `tokio::sync::Mutex` because every `embed` call is pure CPU/GPU work
/// — a tokio mutex held across that synchronous block parks the running
/// task while holding the guard, starving other tokio tasks contending
/// on the same mutex. The parking-lot guard has the same serializing
/// semantics (candle's `BertModel` is `Send` but not `Sync`), but does
/// not pretend to cooperate with the tokio scheduler. No `.await`
/// happens while the guard is alive, so the future remains `Send`.
///
/// Currently the daemon's primary embedding path goes through the
/// dedicated worker pool (`EmbedSource::Pool`) for backpressure, and the
/// CLI uses lazy init (`EmbedSource::Lazy`). `CandleBackend` is the
/// trait-route variant used when callers want to plug a real model into
/// `EmbedSource::Backend(...)` — primarily useful in tests that prefer a
/// real (slow) model over the deterministic mock, or in future production
/// code that bypasses the pool.
#[allow(dead_code)]
pub struct CandleBackend {
    model: parking_lot::Mutex<Embedder>,
}

#[allow(dead_code)]
impl CandleBackend {
    pub fn new(config: &EmbeddingsConfig) -> Result<Self> {
        let model = Embedder::new(config)?;
        Ok(Self {
            model: parking_lot::Mutex::new(model),
        })
    }
}

#[async_trait]
#[allow(dead_code)]
impl EmbeddingBackend for CandleBackend {
    async fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        let guard = self.model.lock();
        let mut vecs = guard.embed(&[text])?;
        vecs.pop()
            .ok_or_else(|| PgmcpError::Embedding("No embedding returned".into()))
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let guard = self.model.lock();
        guard.embed(texts)
    }

    fn name(&self) -> &'static str {
        "candle"
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
