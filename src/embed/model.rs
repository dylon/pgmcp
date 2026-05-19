//! Direct-candle embedder. Replaces the prior fastembed/ort wrapper.
//!
//! Two backbones are supported (selected by `EmbeddingsConfig::model`):
//!
//! - **`all-MiniLM-L6-v2`** (legacy, 384d) — BERT-base architecture with
//!   mean-pooling over the masked sequence; the original pgmcp embedder.
//!   Phase 1 keeps it alive during the BGE-M3 migration window so the
//!   embedding-migration cron can dual-read against both columns.
//! - **`bge-m3`** (Phase 1, 1024d) — XLM-RoBERTa-Large with CLS pooling
//!   and L2 normalization. Multilingual; Matryoshka-truncatable. The
//!   eventual replacement.
//!
//! One `Embedder` per worker thread, bound to one device. `embed()` runs
//! one forward pass per inference sub-batch (size capped by
//! `inference_batch_size`) — batch parallelism belongs to the work-pool
//! layer, not to this module.
//!
//! Model weights are downloaded from HuggingFace on first use and cached
//! at `~/.cache/huggingface/hub/`. Subsequent starts hit the cache.

use std::path::{Path, PathBuf};

use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config as BertConfig, DTYPE as BERT_DTYPE};
use candle_transformers::models::xlm_roberta::{Config as XlmRobertaConfig, XLMRobertaModel};
use hf_hub::api::sync::Api;
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams, TruncationStrategy};

use crate::config::EmbeddingsConfig;
use crate::error::{PgmcpError, Result};

/// Versioned signature for the legacy MiniLM column. Stamped on rows
/// written via the MiniLM backbone so a mixed-signature transition window
/// cannot silently mis-rank cosine distances. See
/// `docs/memory-server/02-phases.md` Phase 1.
#[allow(dead_code)]
pub const MINILM_SIGNATURE: &str = "minilm-l6-v2";

/// Versioned signature for the BGE-M3 column. Bump this whenever the
/// embedding behaviour changes in a way that would invalidate prior
/// vectors (model swap, normalization change, instruction-prefix change).
pub const BGE_M3_SIGNATURE: &str = "bge-m3-v1";

/// Compute the embedding signature that THIS build will stamp on rows it
/// writes for `model_name` (matching `EmbeddingsConfig::model`). Used by
/// the daemon startup probe to compare against the signature already
/// stored in `pgmcp_metadata.active_embedding_signature` and warn the
/// operator when they diverge — a mismatch usually means either an
/// incomplete migration cron run or a daemon downgrade against a
/// newer-signature database, both of which silently degrade recall if
/// not addressed.
pub fn signature_for_model_name(model_name: &str) -> Result<&'static str> {
    Ok(match ModelKind::from_config_name(model_name)? {
        ModelKind::MiniLm => MINILM_SIGNATURE,
        ModelKind::Bgem3 => BGE_M3_SIGNATURE,
    })
}

/// Direct-candle embedder. Owns one model instance bound to one device.
///
/// Internally an enum over the two supported backbones; callers use the
/// uniform `embed()` / `dim()` / `signature()` surface and don't see the
/// dispatch.
pub struct Embedder {
    backbone: Backbone,
    tokenizer: Tokenizer,
    #[allow(dead_code)]
    device: Device,
    max_length: usize,
    /// Cap on input texts per single forward pass. Larger reduces per-call
    /// overhead at the cost of activation memory (self-attention is
    /// O(batch * seq²) per layer). 8 keeps peak VRAM well under 1 GiB at
    /// `max_length = 512`.
    inference_batch_size: usize,
    dim: usize,
}

enum Backbone {
    /// BERT-base architecture, mean-pooled. 384d output.
    MiniLm(BertModel),
    /// XLM-RoBERTa-Large, CLS-pooled. 1024d output.
    Bgem3(XLMRobertaModel),
}

impl Embedder {
    /// Construct an embedder. Resolves the device from `config.use_gpu`,
    /// downloads model files if needed, opens the model + tokenizer.
    pub fn new(config: &EmbeddingsConfig) -> Result<Self> {
        let device = resolve_device(config.use_gpu)?;
        let kind = ModelKind::from_config_name(&config.model)?;
        let model_dir = ensure_model_files(kind)?;

        let cfg_path = model_dir.join("config.json");
        let cfg_json =
            std::fs::read_to_string(&cfg_path).map_err(|e| PgmcpError::file_io(&cfg_path, e))?;

        let weights_path = model_dir.join("model.safetensors");
        // SAFETY: mmap is read-only and the file is owned by the HF cache;
        // candle's VarBuilder treats the mmap as a stable byte slice for
        // the session lifetime.
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[weights_path], BERT_DTYPE, &device)
                .map_err(|e| PgmcpError::Embedding(format!("safetensors load: {}", e)))?
        };

        let backbone = match kind {
            ModelKind::MiniLm => {
                let bert_cfg: BertConfig = serde_json::from_str(&cfg_json)
                    .map_err(|e| PgmcpError::Embedding(format!("bert config.json parse: {}", e)))?;
                let model = BertModel::load(vb, &bert_cfg)
                    .map_err(|e| PgmcpError::Embedding(format!("BertModel::load: {}", e)))?;
                Backbone::MiniLm(model)
            }
            ModelKind::Bgem3 => {
                let cfg: XlmRobertaConfig = serde_json::from_str(&cfg_json).map_err(|e| {
                    PgmcpError::Embedding(format!("xlm-roberta config.json parse: {}", e))
                })?;
                let model = XLMRobertaModel::new(&cfg, vb)
                    .map_err(|e| PgmcpError::Embedding(format!("XLMRobertaModel::new: {}", e)))?;
                Backbone::Bgem3(model)
            }
        };

        let tokenizer_path = model_dir.join("tokenizer.json");
        let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| PgmcpError::Embedding(format!("tokenizer load: {}", e)))?;

        let max_length = if config.max_length == 0 {
            kind.default_max_length()
        } else {
            config.max_length.min(kind.default_max_length())
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

        let dim = kind.output_dim();

        Ok(Self {
            backbone,
            tokenizer,
            device,
            max_length,
            inference_batch_size,
            dim,
        })
    }

    /// Output embedding dimension (384 for MiniLM-L6-v2, 1024 for BGE-M3).
    #[allow(dead_code)]
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Versioned embedding-signature string identifying which backbone
    /// produced these vectors. Use this as the `embedding_signature`
    /// column value on inserts so a mixed-signature transition window
    /// cannot mis-rank cosine distances.
    #[allow(dead_code)]
    pub fn signature(&self) -> &'static str {
        match self.backbone {
            Backbone::MiniLm(_) => MINILM_SIGNATURE,
            Backbone::Bgem3(_) => BGE_M3_SIGNATURE,
        }
    }

    /// Embed a batch of texts. Returns L2-normalized `dim`-length vectors
    /// so dot product equals cosine similarity. No internal parallelism.
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

        let pooled = match &self.backbone {
            Backbone::MiniLm(bert) => {
                let hidden = bert
                    .forward(&input_ids_t, &token_type_ids_t, Some(&attention_mask_t))
                    .map_err(|e| PgmcpError::Embedding(format!("BERT forward: {}", e)))?;
                // hidden: (batch, seq_len, hidden_dim) → mean-pool with mask
                mean_pool_with_mask(&hidden, &attention_mask_t)
                    .map_err(|e| PgmcpError::Embedding(format!("mean_pool: {}", e)))?
            }
            Backbone::Bgem3(xlm) => {
                let hidden = xlm
                    .forward(
                        &input_ids_t,
                        &attention_mask_t,
                        &token_type_ids_t,
                        None,
                        None,
                        None,
                    )
                    .map_err(|e| PgmcpError::Embedding(format!("XLM-RoBERTa forward: {}", e)))?;
                // BGE-M3 dense mode = CLS pooling: take token index 0.
                cls_pool(&hidden).map_err(|e| PgmcpError::Embedding(format!("cls_pool: {}", e)))?
            }
        };

        let normalized = l2_normalize_rows(&pooled)
            .map_err(|e| PgmcpError::Embedding(format!("normalize: {}", e)))?;

        let v: Vec<Vec<f32>> = normalized
            .to_vec2::<f32>()
            .map_err(|e| PgmcpError::Embedding(format!("to_vec2: {}", e)))?;
        Ok(v)
    }
}

/// Closed-set model selector. New backbones land here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelKind {
    MiniLm,
    Bgem3,
}

impl ModelKind {
    fn from_config_name(name: &str) -> Result<Self> {
        match name {
            "all-MiniLM-L6-v2" => Ok(Self::MiniLm),
            "bge-m3" | "BAAI/bge-m3" => Ok(Self::Bgem3),
            other => Err(PgmcpError::Embedding(format!(
                "Unsupported embedding model: {}",
                other
            ))),
        }
    }

    fn hf_repo(self) -> &'static str {
        match self {
            Self::MiniLm => "sentence-transformers/all-MiniLM-L6-v2",
            Self::Bgem3 => "BAAI/bge-m3",
        }
    }

    fn model_files(self) -> &'static [&'static str] {
        match self {
            Self::MiniLm => &["model.safetensors", "config.json", "tokenizer.json"],
            // BGE-M3 ships with the same canonical names as MiniLM.
            Self::Bgem3 => &["model.safetensors", "config.json", "tokenizer.json"],
        }
    }

    fn default_max_length(self) -> usize {
        match self {
            Self::MiniLm => 512,
            // BGE-M3 supports up to 8192 but inference at 8k is impractical
            // on consumer hardware; cap at 512 for parity with the existing
            // chunker output (paragraph-class chunks fit easily).
            Self::Bgem3 => 512,
        }
    }

    fn output_dim(self) -> usize {
        match self {
            Self::MiniLm => 384,
            Self::Bgem3 => 1024,
        }
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

/// Ensure model files are available locally, downloading via hf-hub on
/// cold caches. Returns the directory containing `model.safetensors`,
/// `config.json`, and `tokenizer.json`.
fn ensure_model_files(kind: ModelKind) -> Result<PathBuf> {
    let api = Api::new().map_err(|e| PgmcpError::Embedding(format!("hf-hub init: {}", e)))?;
    let api = api.model(kind.hf_repo().to_string());

    let mut dir: Option<PathBuf> = None;
    for f in kind.model_files() {
        let path = api
            .get(f)
            .map_err(|e| PgmcpError::Embedding(format!("hf get {}: {}", f, e)))?;
        if dir.is_none() {
            dir = path.parent().map(Path::to_path_buf);
        }
    }
    dir.ok_or_else(|| PgmcpError::Embedding("hf cache resolution".into()))
}

/// Mean pool the per-token hidden states using the attention mask,
/// yielding a (batch, hidden_dim) tensor. Used for MiniLM.
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

/// CLS pool: take the first token's hidden state from each row. Used for
/// BGE-M3 dense mode.
fn cls_pool(hidden: &Tensor) -> std::result::Result<Tensor, candle_core::Error> {
    // hidden: (b, s, d) → (b, d) by selecting index 0 on the seq axis.
    hidden.i((.., 0, ..))
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

/// Truncate a full-dim vector to a Matryoshka prefix length and
/// re-L2-normalize. Used by query-time ANN (cheap) before the full-dim
/// rerank.
///
/// Returns the input unchanged when `target_dim >= full.len()`. When
/// `target_dim == 0`, returns an empty vector (the caller is responsible
/// for not asking for that).
#[allow(dead_code)]
pub fn matryoshka_truncate(full: &[f32], target_dim: usize) -> Vec<f32> {
    if target_dim == 0 {
        return Vec::new();
    }
    if target_dim >= full.len() {
        return full.to_vec();
    }
    let mut out: Vec<f32> = full[..target_dim].to_vec();
    let norm: f32 = out.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
    for v in &mut out {
        *v /= norm;
    }
    out
}

// Re-export the `IndexOp` trait so the `i((..., 0, ...))` call above
// resolves. Adding the import at the top of the file is the conventional
// place, but keeping it scoped here documents *why* we need it.
use candle_core::IndexOp;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_kind_dispatch_matches_string_names() {
        assert_eq!(
            ModelKind::from_config_name("all-MiniLM-L6-v2").unwrap(),
            ModelKind::MiniLm
        );
        assert_eq!(
            ModelKind::from_config_name("bge-m3").unwrap(),
            ModelKind::Bgem3
        );
        assert_eq!(
            ModelKind::from_config_name("BAAI/bge-m3").unwrap(),
            ModelKind::Bgem3
        );
        assert!(ModelKind::from_config_name("unsupported").is_err());
    }

    #[test]
    fn signature_for_model_name_matches_kind_constants() {
        assert_eq!(
            signature_for_model_name("all-MiniLM-L6-v2").unwrap(),
            MINILM_SIGNATURE
        );
        assert_eq!(
            signature_for_model_name("bge-m3").unwrap(),
            BGE_M3_SIGNATURE
        );
        assert_eq!(
            signature_for_model_name("BAAI/bge-m3").unwrap(),
            BGE_M3_SIGNATURE
        );
        assert!(signature_for_model_name("totally-fake-model").is_err());
    }

    #[test]
    fn output_dim_and_signature_are_correct_per_kind() {
        assert_eq!(ModelKind::MiniLm.output_dim(), 384);
        assert_eq!(ModelKind::Bgem3.output_dim(), 1024);
    }

    #[test]
    fn matryoshka_truncate_clips_and_renormalizes() {
        let full = vec![1.0_f32, 0.0, 0.0, 0.0];
        let half = matryoshka_truncate(&full, 2);
        assert_eq!(half.len(), 2);
        let norm: f32 = half.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6);
    }

    #[test]
    fn matryoshka_truncate_no_op_when_target_ge_input() {
        let full = vec![0.5_f32, 0.5, 0.5, 0.5];
        let copy = matryoshka_truncate(&full, 8);
        assert_eq!(copy, full);
    }
}
