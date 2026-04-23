//! Embedding model wrapper using fastembed.

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

use crate::config::EmbeddingsConfig;
use crate::error::{PgmcpError, Result};

/// Create and initialize a TextEmbedding model.
pub fn create_embedding_model(config: &EmbeddingsConfig) -> Result<TextEmbedding> {
    let model = match config.model.as_str() {
        "all-MiniLM-L6-v2" => EmbeddingModel::AllMiniLML6V2,
        other => {
            return Err(PgmcpError::Embedding(format!(
                "Unsupported embedding model: {}",
                other
            )));
        }
    };

    let options = InitOptions::new(model).with_show_download_progress(true);

    let embedding = TextEmbedding::try_new(options).map_err(|e| {
        PgmcpError::Embedding(format!("Failed to initialize embedding model: {}", e))
    })?;

    Ok(embedding)
}

/// Embed a batch of texts.
pub fn embed_batch(model: &TextEmbedding, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
    let embeddings = model
        .embed(texts.to_vec(), None)
        .map_err(|e| PgmcpError::Embedding(format!("Embedding failed: {}", e)))?;
    Ok(embeddings)
}

/// Embed a single text.
pub fn embed_single(model: &TextEmbedding, text: &str) -> Result<Vec<f32>> {
    let embeddings = model
        .embed(vec![text], None)
        .map_err(|e| PgmcpError::Embedding(format!("Embedding failed: {}", e)))?;
    embeddings
        .into_iter()
        .next()
        .ok_or_else(|| PgmcpError::Embedding("No embedding returned".into()))
}
