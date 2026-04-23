#[allow(dead_code)]
pub mod model;
pub mod pool;

use std::sync::Arc;

use crate::config::EmbeddingsConfig;
use crate::error::{PgmcpError, Result};

/// Source for query-time embeddings. Abstracts daemon (pool) vs CLI (lazy) modes.
#[derive(Clone)]
pub enum EmbedSource {
    /// Daemon mode: route through the embed pool's priority query channel.
    Pool(pool::QueryEmbedder),
    /// CLI mode: lazily create a local model on first use (no pool running).
    Lazy {
        cell: Arc<tokio::sync::OnceCell<Arc<tokio::sync::Mutex<fastembed::TextEmbedding>>>>,
        config: EmbeddingsConfig,
    },
}

impl EmbedSource {
    /// Convenience constructor for CLI lazy mode.
    pub fn lazy(config: EmbeddingsConfig) -> Self {
        Self::Lazy {
            cell: Arc::new(tokio::sync::OnceCell::new()),
            config,
        }
    }

    /// Embed a single query string.
    pub async fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        match self {
            Self::Pool(embedder) => embedder.embed_query(text.to_string()).await,
            Self::Lazy { cell, config } => {
                let model_arc = cell
                    .get_or_try_init(|| async {
                        let m = model::create_embedding_model(config)?;
                        Ok::<_, PgmcpError>(Arc::new(tokio::sync::Mutex::new(m)))
                    })
                    .await?;
                let guard = model_arc.lock().await;
                let mut vecs = guard
                    .embed(vec![text], None)
                    .map_err(|e| PgmcpError::Embedding(format!("Embedding failed: {}", e)))?;
                vecs.pop()
                    .ok_or_else(|| PgmcpError::Embedding("No embedding returned".into()))
            }
        }
    }
}
