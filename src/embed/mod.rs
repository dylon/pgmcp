pub mod backend;
pub mod model;
pub mod pool;

pub use backend::EmbeddingBackend;
// CandleBackend re-exported for callers (incl. tests) wanting to plug
// the real model into EmbedSource::Backend(...). Not yet used by the
// daemon's primary embed path (which goes through EmbedSource::Pool).
#[allow(unused_imports)]
pub use backend::CandleBackend;
pub use model::Embedder;

use std::sync::Arc;

use crate::config::EmbeddingsConfig;
use crate::error::{PgmcpError, Result};

/// Source for query-time embeddings. Abstracts daemon (pool) vs CLI (lazy)
/// modes, plus the runtime-injectable backend used in tests.
#[derive(Clone)]
pub enum EmbedSource {
    /// Daemon mode: route through the embed pool's priority query channel.
    Pool(pool::QueryEmbedder),
    /// CLI mode: lazily create a local model on first use (no pool running).
    /// The model is held behind a `parking_lot::Mutex` so the synchronous
    /// `embed` call inside `embed_query` doesn't park the calling tokio
    /// task with a tokio mutex held — see the rationale on
    /// [`backend::CandleBackend`].
    Lazy {
        cell: Arc<tokio::sync::OnceCell<Arc<parking_lot::Mutex<Embedder>>>>,
        config: EmbeddingsConfig,
    },
    /// Direct trait dispatch — production wraps `CandleBackend`; tests
    /// wrap `DeterministicEmbeddingBackend` from `pgmcp-testing`. Not yet
    /// constructed by daemon-mode `main.rs`, but consumed end-to-end by
    /// `pgmcp-testing/tests/mcp_tool_smoke.rs`.
    #[allow(dead_code)]
    Backend(Arc<dyn EmbeddingBackend>),
}

impl EmbedSource {
    /// Convenience constructor for CLI lazy mode.
    pub fn lazy(config: EmbeddingsConfig) -> Self {
        Self::Lazy {
            cell: Arc::new(tokio::sync::OnceCell::new()),
            config,
        }
    }

    /// Wrap a trait-object embedding backend. Tests pass
    /// `DeterministicEmbeddingBackend`; production code that wants to
    /// bypass the pool can pass `CandleBackend`. Currently used only by
    /// the `pgmcp-testing` cross-crate tests.
    #[allow(dead_code)]
    pub fn backend(backend: Arc<dyn EmbeddingBackend>) -> Self {
        Self::Backend(backend)
    }

    /// Embed a single query string.
    pub async fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        match self {
            Self::Pool(embedder) => embedder.embed_query(text.to_string()).await,
            Self::Lazy { cell, config } => {
                let model_arc = cell
                    .get_or_try_init(|| async {
                        let m = Embedder::new(config)?;
                        Ok::<_, PgmcpError>(Arc::new(parking_lot::Mutex::new(m)))
                    })
                    .await?;
                let guard = model_arc.lock();
                let mut vecs = guard.embed(&[text])?;
                vecs.pop()
                    .ok_or_else(|| PgmcpError::Embedding("No embedding returned".into()))
            }
            Self::Backend(b) => b.embed_one(text).await,
        }
    }
}
