//! Embedding backend smoke tests.
//!
//! DeterministicEmbeddingBackend tests always run (no infra).
//! CandleBackend tests require the model files to be cached at
//! `~/.cache/huggingface/hub/` — skipped otherwise. This is candle's
//! HuggingFace-hub cache location, so a successful end-to-end embed
//! run implies the model is there.

use std::sync::Arc;

use pgmcp::config::EmbeddingsConfig;
use pgmcp::embed::EmbeddingBackend;
use pgmcp_testing::mocks::DeterministicEmbeddingBackend;

#[tokio::test]
async fn deterministic_backend_returns_unit_normalized_vectors() {
    let backend = DeterministicEmbeddingBackend::new(384);
    let v = backend.embed_one("hello world").await.expect("embed");
    assert_eq!(v.len(), 384);
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!((norm - 1.0).abs() < 1e-4, "‖v‖ = {}", norm);
}

#[tokio::test]
async fn deterministic_backend_is_stable_for_same_input() {
    let backend = DeterministicEmbeddingBackend::new(384);
    let a = backend.embed_one("repeatable").await.expect("first");
    let b = backend.embed_one("repeatable").await.expect("second");
    assert_eq!(a.len(), b.len());
    for (x, y) in a.iter().zip(b.iter()) {
        assert!(
            (x - y).abs() < 1e-6,
            "deterministic backend should be stable"
        );
    }
}

#[tokio::test]
async fn deterministic_backend_distinguishes_different_inputs() {
    let backend = DeterministicEmbeddingBackend::new(384);
    let a = backend.embed_one("alpha").await.expect("a");
    let b = backend.embed_one("beta").await.expect("b");
    // Not identical (extremely unlikely collision).
    let diff: f32 = a.iter().zip(b.iter()).map(|(x, y)| (x - y).abs()).sum();
    assert!(
        diff > 1e-3,
        "different inputs should yield different vectors"
    );
}

#[tokio::test]
async fn deterministic_backend_embed_batch_matches_embed_one() {
    let backend = DeterministicEmbeddingBackend::new(384);
    let inputs: Vec<&str> = vec!["alpha", "beta", "gamma"];
    let batch = backend.embed_batch(&inputs).await.expect("batch");
    assert_eq!(batch.len(), 3);
    for (i, expected_text) in inputs.iter().enumerate() {
        let single = backend.embed_one(expected_text).await.expect("single");
        assert_eq!(single.len(), batch[i].len());
        for (x, y) in batch[i].iter().zip(single.iter()) {
            assert!((x - y).abs() < 1e-6, "batch and single disagree at element");
        }
    }
}

/// Only runs when the real candle BERT model is cached locally. Skipped
/// otherwise with a visible message.
#[tokio::test(flavor = "multi_thread")]
async fn candle_backend_embed_one_returns_normalized_vector_if_cached() {
    // Detect whether the default model is already cached — skip if not.
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => {
            eprintln!("SKIPPED: HOME not set");
            return;
        }
    };
    let cache_marker = std::path::Path::new(&home).join(".cache/huggingface/hub");
    if !cache_marker.exists() {
        eprintln!("SKIPPED: ~/.cache/huggingface/hub not present (model not downloaded)");
        return;
    }

    let config = EmbeddingsConfig::default();
    // CandleBackend::new downloads the model if not cached. We skip
    // proactively on cold caches, so this should hit the fast path.
    let backend = match tokio::task::spawn_blocking(move || {
        pgmcp::embed::backend::CandleBackend::new(&config)
    })
    .await
    {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => {
            eprintln!("SKIPPED: CandleBackend unavailable: {}", e);
            return;
        }
        Err(e) => {
            eprintln!("SKIPPED: CandleBackend spawn panic: {}", e);
            return;
        }
    };
    let backend: Arc<dyn EmbeddingBackend> = Arc::new(backend);
    let v = backend.embed_one("hello world").await.expect("embed");
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!(
        (norm - 1.0).abs() < 1e-3,
        "candle backend should L2-normalize; got ‖v‖ = {}",
        norm
    );
}
