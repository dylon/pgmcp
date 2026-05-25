//! F1 — `Embedder::new` for `model = "bge-m3"` loads weights from
//! `pytorch_model.bin` via `VarBuilder::from_pth` (NOT from
//! `model.safetensors`, which BAAI/bge-m3 does not publish).
//!
//! Regression for the 20× HTTP-404 log records in
//! `~/.local/share/pgmcp/pgmcp.log`:
//!
//!   Embedder::new failed; supervisor will retry — Embedding error:
//!   hf get model.safetensors: request error:
//!   https://huggingface.co/BAAI/bge-m3/resolve/main/model.safetensors:
//!   status code 404
//!
//! Plan: `~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md` F1.
//!
//! Skip semantics: this test downloads ~2 GB of model weights on a cold
//! HF cache. It is automatically skipped when:
//!   - `BGE_M3_TEST_SKIP_DOWNLOAD=1` is set in the environment, OR
//!   - `~/.cache/huggingface/hub/models--BAAI--bge-m3/` does not exist
//!     (cold cache; do not pay the download cost in CI).
//! In both cases the test prints a `SKIPPED:` line and returns Ok.

use std::sync::Arc;

use pgmcp::config::EmbeddingsConfig;
use pgmcp::embed::EmbeddingBackend;

fn bge_m3_config() -> EmbeddingsConfig {
    EmbeddingsConfig {
        model: "bge-m3".into(),
        dimensions: 1024,
        use_gpu: false,
        ..EmbeddingsConfig::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn bgem3_candle_backend_loads_and_embeds_to_1024d() {
    if std::env::var("BGE_M3_TEST_SKIP_DOWNLOAD").is_ok() {
        eprintln!("SKIPPED: BGE_M3_TEST_SKIP_DOWNLOAD=1");
        return;
    }
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => {
            eprintln!("SKIPPED: HOME not set");
            return;
        }
    };
    let cache_marker =
        std::path::Path::new(&home).join(".cache/huggingface/hub/models--BAAI--bge-m3");
    if !cache_marker.exists() {
        eprintln!(
            "SKIPPED: {} not present (cold cache; pre-warm before running this test)",
            cache_marker.display()
        );
        return;
    }

    let config = bge_m3_config();
    let backend = match tokio::task::spawn_blocking(move || {
        pgmcp::embed::backend::CandleBackend::new(&config)
    })
    .await
    {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => {
            panic!(
                "BGE-M3 CandleBackend::new failed (cache present at {}, so this is a real \
                 loader bug — likely a regression of F1): {}",
                cache_marker.display(),
                e
            );
        }
        Err(e) => panic!("BGE-M3 CandleBackend spawn panic: {}", e),
    };

    let backend: Arc<dyn EmbeddingBackend> = Arc::new(backend);
    let v = backend
        .embed_one("hello world")
        .await
        .expect("embed_one should succeed once Embedder::new returns Ok");

    assert_eq!(
        v.len(),
        1024,
        "BGE-M3 produces 1024-dim embeddings; got {}",
        v.len()
    );
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!(
        (norm - 1.0).abs() < 1e-3,
        "BGE-M3 output is L2-normalized in the CLS-pool path; ‖v‖ = {}",
        norm
    );
}
