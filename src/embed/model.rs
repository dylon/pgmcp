//! Direct-candle BERT embedder. Replaces the prior fastembed/ort wrapper.
//!
//! One `Embedder` per worker thread. Owns a `BertModel`, a `Tokenizer`,
//! and a `Device` (`Cpu` or `Cuda(0)`). `embed()` runs one forward pass
//! per batch with no internal parallelism — batch parallelism belongs to
//! the work-pool layer, not to the embedding library.
//!
//! All-MiniLM-L6-v2 weights are downloaded from HuggingFace on first use
//! and cached at `~/.cache/huggingface/hub/`. Subsequent starts hit the
//! cache.

use std::path::{Path, PathBuf};

use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config, DTYPE as BERT_DTYPE};
use hf_hub::api::sync::Api;
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams, TruncationStrategy};

use crate::config::EmbeddingsConfig;
use crate::error::{PgmcpError, Result};

const HF_REPO: &str = "sentence-transformers/all-MiniLM-L6-v2";
const MODEL_FILES: &[&str] = &["model.safetensors", "config.json", "tokenizer.json"];

/// Direct candle BERT embedder. Owns one model instance bound to one device.
pub struct Embedder {
    model: BertModel,
    tokenizer: Tokenizer,
    #[allow(dead_code)]
    device: Device,
    max_length: usize,
    /// Cap on input texts per single forward pass. Larger reduces per-call
    /// overhead at the cost of activation memory (BERT self-attention is
    /// O(batch * seq²) per layer). 8 keeps peak VRAM well under 1 GiB at
    /// `max_length = 512` per worker.
    inference_batch_size: usize,
    dim: usize,
}

impl Embedder {
    /// Construct an embedder. Resolves the device from `config.use_gpu`,
    /// downloads model files if needed, opens the model and tokenizer.
    pub fn new(config: &EmbeddingsConfig) -> Result<Self> {
        let device = resolve_device(config.use_gpu)?;
        let model_dir = ensure_model_files(&config.model)?;

        let cfg_path = model_dir.join("config.json");
        let cfg_json =
            std::fs::read_to_string(&cfg_path).map_err(|e| PgmcpError::file_io(&cfg_path, e))?;
        let bert_config: Config = serde_json::from_str(&cfg_json)
            .map_err(|e| PgmcpError::Embedding(format!("config.json parse: {}", e)))?;

        let weights_path = model_dir.join("model.safetensors");
        // SAFETY: mmap is read-only and the file is owned by the HF cache;
        // candle's VarBuilder treats the mmap as a stable byte slice for the
        // session lifetime.
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[weights_path], BERT_DTYPE, &device)
                .map_err(|e| PgmcpError::Embedding(format!("safetensors load: {}", e)))?
        };

        let model = BertModel::load(vb, &bert_config)
            .map_err(|e| PgmcpError::Embedding(format!("BertModel::load: {}", e)))?;

        let tokenizer_path = model_dir.join("tokenizer.json");
        let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| PgmcpError::Embedding(format!("tokenizer load: {}", e)))?;

        let max_length = if config.max_length == 0 {
            512
        } else {
            config.max_length
        };

        tokenizer.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            ..Default::default()
        }));
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length,
                strategy: TruncationStrategy::LongestFirst,
                ..Default::default()
            }))
            .map_err(|e| PgmcpError::Embedding(format!("tokenizer truncation: {}", e)))?;

        let inference_batch_size = if config.inference_batch_size == 0 {
            8
        } else {
            config.inference_batch_size
        };

        Ok(Self {
            model,
            tokenizer,
            device,
            max_length,
            inference_batch_size,
            dim: config.dimensions,
        })
    }

    /// Output embedding dimension (384 for all-MiniLM-L6-v2).
    #[allow(dead_code)]
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Embed a batch of texts. Returns L2-normalized `dim`-length vectors so
    /// dot product equals cosine similarity. No internal parallelism.
    ///
    /// Internally splits the input into forward-pass-sized sub-batches of
    /// `inference_batch_size` chunks. BERT self-attention allocates an
    /// `(batch, heads, seq, seq)` matrix per layer; at `max_length = 512`
    /// and `heads = 12`, one attention layer's matrix is `batch * 1.5 MiB`
    /// — so unbounded batches OOM the GPU on files with many chunks. The
    /// sub-batch loop bounds peak working set to a function of
    /// `inference_batch_size * max_length²`.
    pub fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let mut out: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(self.inference_batch_size) {
            let mut sub = self.embed_one_batch(chunk)?;
            out.append(&mut sub);
        }
        Ok(out)
    }

    /// Run one forward pass for an entire input slice (no further sub-batching).
    fn embed_one_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let owned: Vec<String> = texts.iter().map(|s| (*s).to_string()).collect();
        let encodings = self
            .tokenizer
            .encode_batch(owned, true)
            .map_err(|e| PgmcpError::Embedding(format!("encode_batch: {}", e)))?;

        let batch_size = encodings.len();
        let seq_len = encodings
            .iter()
            .map(|e| e.get_ids().len())
            .max()
            .unwrap_or(0)
            .min(self.max_length);

        if seq_len == 0 {
            // All inputs were empty / produced no tokens; emit zero vectors.
            return Ok(vec![vec![0.0_f32; self.dim]; batch_size]);
        }

        let mut input_ids: Vec<i64> = Vec::with_capacity(batch_size * seq_len);
        let mut attention_mask: Vec<i64> = Vec::with_capacity(batch_size * seq_len);
        let mut token_type_ids: Vec<i64> = Vec::with_capacity(batch_size * seq_len);
        for enc in &encodings {
            let ids = enc.get_ids();
            let mask = enc.get_attention_mask();
            let types = enc.get_type_ids();
            let len = ids.len().min(seq_len);
            for j in 0..len {
                input_ids.push(ids[j] as i64);
                attention_mask.push(mask[j] as i64);
                token_type_ids.push(types[j] as i64);
            }
            for _ in len..seq_len {
                input_ids.push(0);
                attention_mask.push(0);
                token_type_ids.push(0);
            }
        }

        let shape = (batch_size, seq_len);
        let input_ids_t = Tensor::from_vec(input_ids, shape, &self.device)
            .map_err(|e| PgmcpError::Embedding(format!("input_ids tensor: {}", e)))?;
        let attention_mask_t = Tensor::from_vec(attention_mask, shape, &self.device)
            .map_err(|e| PgmcpError::Embedding(format!("attention_mask tensor: {}", e)))?;
        let token_type_ids_t = Tensor::from_vec(token_type_ids, shape, &self.device)
            .map_err(|e| PgmcpError::Embedding(format!("token_type_ids tensor: {}", e)))?;

        let hidden = self
            .model
            .forward(&input_ids_t, &token_type_ids_t, Some(&attention_mask_t))
            .map_err(|e| PgmcpError::Embedding(format!("forward: {}", e)))?;
        // hidden: (batch, seq_len, hidden_dim)

        let pooled = mean_pool_with_mask(&hidden, &attention_mask_t)
            .map_err(|e| PgmcpError::Embedding(format!("mean_pool: {}", e)))?;
        let normalized = l2_normalize_rows(&pooled)
            .map_err(|e| PgmcpError::Embedding(format!("normalize: {}", e)))?;

        let v: Vec<Vec<f32>> = normalized
            .to_vec2::<f32>()
            .map_err(|e| PgmcpError::Embedding(format!("to_vec2: {}", e)))?;
        Ok(v)
    }
}

/// Resolve a candle `Device` from the `use_gpu` config flag. When true,
/// open `Cuda(0)`; on init failure, surface the error so the caller can
/// fail loudly rather than silently degrade.
fn resolve_device(use_gpu: bool) -> Result<Device> {
    if use_gpu {
        Device::new_cuda(0).map_err(|e| PgmcpError::Embedding(format!("CUDA init failed: {}", e)))
    } else {
        Ok(Device::Cpu)
    }
}

/// Resolve the HuggingFace repo for a configured model name.
fn hf_repo_for(model_name: &str) -> Result<&'static str> {
    match model_name {
        "all-MiniLM-L6-v2" => Ok(HF_REPO),
        other => Err(PgmcpError::Embedding(format!(
            "Unsupported embedding model: {}",
            other
        ))),
    }
}

/// Ensure model files are available locally, downloading via hf-hub on cold
/// caches. Returns the directory containing `model.safetensors`,
/// `config.json`, and `tokenizer.json`.
fn ensure_model_files(model_name: &str) -> Result<PathBuf> {
    let repo = hf_repo_for(model_name)?;
    let api = Api::new().map_err(|e| PgmcpError::Embedding(format!("hf-hub init: {}", e)))?;
    let api = api.model(repo.to_string());

    let mut dir: Option<PathBuf> = None;
    for f in MODEL_FILES {
        let path = api
            .get(f)
            .map_err(|e| PgmcpError::Embedding(format!("hf get {}: {}", f, e)))?;
        if dir.is_none() {
            dir = path.parent().map(Path::to_path_buf);
        }
    }
    dir.ok_or_else(|| PgmcpError::Embedding("hf cache resolution".into()))
}

/// Mean pool the per-token hidden states using the attention mask, yielding
/// a (batch, hidden_dim) tensor.
fn mean_pool_with_mask(
    hidden: &Tensor,
    mask: &Tensor,
) -> std::result::Result<Tensor, candle_core::Error> {
    // hidden: (b, s, d); mask: (b, s)
    let mask = mask.to_dtype(DType::F32)?.unsqueeze(2)?; // (b, s, 1)
    let masked = hidden.broadcast_mul(&mask)?; // (b, s, d)
    let summed = masked.sum(1)?; // (b, d)
    let counts = mask.sum(1)?.clamp(1f32, f32::INFINITY)?; // (b, 1)
    summed.broadcast_div(&counts)
}

/// L2-normalize each row of a 2-D tensor.
fn l2_normalize_rows(t: &Tensor) -> std::result::Result<Tensor, candle_core::Error> {
    let norms = t
        .sqr()?
        .sum_keepdim(1)?
        .sqrt()?
        .clamp(1e-12f32, f32::INFINITY)?;
    t.broadcast_div(&norms)
}
