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
use candle_nn::{Linear, Module, VarBuilder};
use candle_transformers::models::bert::{BertModel, Config as BertConfig, DTYPE as BERT_DTYPE};
use candle_transformers::models::xlm_roberta::{Config as XlmRobertaConfig, XLMRobertaModel};
use hf_hub::api::sync::Api;
use pgvector::SparseVector;
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
    /// BGE-M3 learned-sparse (SPLADE-style) projection head, loaded from the
    /// checkpoint's `sparse_linear` (hidden→1). `None` for MiniLm or when the
    /// head is absent — the sparse leg is then simply unavailable (additive;
    /// dense + BM25 retrieval is unaffected). (graph-roadmap Phase 2.3)
    sparse_linear: Option<Linear>,
    /// Vocabulary size = dimensionality of the sparse vector (token-id space).
    sparse_dim: usize,
    /// BGE-M3 ColBERT multi-vector projection head (`colbert_linear`,
    /// hidden→hidden). `None` for MiniLm / when absent. Produces per-token
    /// vectors for late-interaction (MaxSim) reranking (graph-roadmap Phase 2.5).
    colbert_linear: Option<Linear>,
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

        let vb = match kind {
            ModelKind::MiniLm => {
                let weights_path = model_dir.join("model.safetensors");
                // SAFETY: mmap is read-only and the file is owned by the HF cache;
                // candle's VarBuilder treats the mmap as a stable byte slice for
                // the session lifetime.
                unsafe {
                    VarBuilder::from_mmaped_safetensors(&[weights_path], BERT_DTYPE, &device)
                        .map_err(|e| PgmcpError::Embedding(format!("safetensors load: {}", e)))?
                }
            }
            ModelKind::Bgem3 => {
                let weights_path = model_dir.join("pytorch_model.bin");
                // BGE-M3 (XLM-RoBERTa-Large, ~560M params) at F32 is
                // ~2.24 GiB per worker; pool_size = 2 plus activations
                // exceeds the 8 GiB VRAM budget on the project's
                // reference hardware (RTX 4060 Ti, SM 8.9). On CUDA
                // we load at BF16: halves weight + activation memory,
                // same dynamic range as F32 (8-bit exponent) so
                // attention softmax / LayerNorm are numerically
                // safer than F16. On CPU fallback we stay at F32 —
                // candle's CPU matmul does not implement BF16
                // ("unsupported dtype BF16 for op matmul") and there's
                // no VRAM constraint to motivate the precision drop.
                // See ~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md
                // F5.
                let dtype = if device.is_cuda() {
                    DType::BF16
                } else {
                    BERT_DTYPE
                };
                VarBuilder::from_pth(&weights_path, dtype, &device)
                    .map_err(|e| PgmcpError::Embedding(format!("pth load ({:?}): {}", dtype, e)))?
            }
        };

        // Optional BGE-M3 sparse head + its dimensionality (vocab). Set inside
        // the Bgem3 arm before `vb` is moved into the model.
        let mut sparse_linear: Option<Linear> = None;
        let mut sparse_dim: usize = 0;
        let mut colbert_linear: Option<Linear> = None;
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
                // Load `sparse_linear` (hidden→1) from the same checkpoint if
                // present. Graceful: `.ok()` so a missing/odd head just leaves
                // the sparse leg unavailable rather than failing the embedder.
                // `vb.pp(..)` borrows; `vb` is still moved into the model below.
                sparse_linear = candle_nn::linear(cfg.hidden_size, 1, vb.pp("sparse_linear"))
                    .or_else(|_| {
                        candle_nn::linear_no_bias(cfg.hidden_size, 1, vb.pp("sparse_linear"))
                    })
                    .ok();
                sparse_dim = cfg.vocab_size;
                // ColBERT head (hidden→hidden); optional/graceful (Phase 2.5).
                colbert_linear =
                    candle_nn::linear(cfg.hidden_size, cfg.hidden_size, vb.pp("colbert_linear"))
                        .or_else(|_| {
                            candle_nn::linear_no_bias(
                                cfg.hidden_size,
                                cfg.hidden_size,
                                vb.pp("colbert_linear"),
                            )
                        })
                        .ok();
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
            sparse_linear,
            sparse_dim,
            colbert_linear,
        })
    }

    /// `true` when this embedder can produce BGE-M3 ColBERT per-token vectors.
    pub fn has_colbert(&self) -> bool {
        self.colbert_linear.is_some()
    }

    /// Compute BGE-M3 ColBERT per-token vectors for `texts` — one
    /// `Some(Vec<token_vector>)` per text (each token vector L2-normalized,
    /// `hidden`-dim), or `None` when no ColBERT head is loaded. Used for
    /// late-interaction (MaxSim) reranking. NUMERICS GPU/deployment-verified.
    pub fn embed_colbert(&self, texts: &[&str]) -> Result<Vec<Option<Vec<Vec<f32>>>>> {
        if !self.has_colbert() {
            return Ok(vec![None; texts.len()]);
        }
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut out: Vec<Option<Vec<Vec<f32>>>> = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(self.inference_batch_size) {
            out.append(&mut self.colbert_one_batch(chunk)?);
        }
        Ok(out)
    }

    fn colbert_one_batch(&self, texts: &[&str]) -> Result<Vec<Option<Vec<Vec<f32>>>>> {
        let (Some(colbert_linear), Backbone::Bgem3(xlm)) = (&self.colbert_linear, &self.backbone)
        else {
            return Ok(vec![None; texts.len()]);
        };
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
            return Ok(vec![None; batch_size]);
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
        let input_ids_t = Tensor::from_vec(input_ids.clone(), shape, &self.device)
            .map_err(|e| PgmcpError::Embedding(format!("input_ids tensor: {}", e)))?;
        let attention_mask_t = Tensor::from_vec(attention_mask.clone(), shape, &self.device)
            .map_err(|e| PgmcpError::Embedding(format!("attention_mask tensor: {}", e)))?;
        let token_type_ids_t = Tensor::from_vec(token_type_ids, shape, &self.device)
            .map_err(|e| PgmcpError::Embedding(format!("token_type_ids tensor: {}", e)))?;

        let hidden = xlm
            .forward(
                &input_ids_t,
                &attention_mask_t,
                &token_type_ids_t,
                None,
                None,
                None,
            )
            .map_err(|e| PgmcpError::Embedding(format!("XLM-RoBERTa forward (colbert): {}", e)))?;
        // (b, s, h) → colbert_linear → (b, s, h), F32.
        let proj = colbert_linear
            .forward(&hidden)
            .and_then(|t| t.to_dtype(DType::F32))
            .map_err(|e| PgmcpError::Embedding(format!("colbert projection: {}", e)))?;
        let m: Vec<Vec<Vec<f32>>> = proj
            .to_vec3::<f32>()
            .map_err(|e| PgmcpError::Embedding(format!("colbert to_vec3: {}", e)))?;

        let mut result: Vec<Option<Vec<Vec<f32>>>> = Vec::with_capacity(batch_size);
        for (r, rows) in m.iter().enumerate() {
            let mut toks: Vec<Vec<f32>> = Vec::new();
            for (t, vec) in rows.iter().enumerate().take(seq_len) {
                let idx = r * seq_len + t;
                if attention_mask[idx] == 0 {
                    continue; // padding
                }
                if (0..=3).contains(&input_ids[idx]) {
                    continue; // XLM-R special tokens
                }
                // L2-normalize each token vector so MaxSim dot == cosine.
                let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
                if norm > 1e-12 {
                    toks.push(vec.iter().map(|x| x / norm).collect());
                }
            }
            result.push(if toks.is_empty() { None } else { Some(toks) });
        }
        Ok(result)
    }

    /// `true` when this embedder can produce BGE-M3 learned-sparse vectors.
    pub fn has_sparse(&self) -> bool {
        self.sparse_linear.is_some()
    }

    /// Compute BGE-M3 learned-sparse (SPLADE-style) vectors for `texts` — one
    /// `Some(SparseVector)` per text, or `None` when no sparse head is loaded
    /// or the text produced no salient tokens. Sub-batches like [`Self::embed`].
    ///
    /// NUMERICS (validated on GPU per the roadmap's "implement fully now"
    /// decision): `relu(sparse_linear(hidden))` per token, then scatter-max of
    /// the weight into the token's vocab slot, skipping XLM-R special tokens
    /// (`<s>`,`<pad>`,`</s>`,`<unk>` = ids 0..3) and padding.
    pub fn embed_sparse(&self, texts: &[&str]) -> Result<Vec<Option<SparseVector>>> {
        if self.sparse_linear.is_none() {
            return Ok(vec![None; texts.len()]);
        }
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut out: Vec<Option<SparseVector>> = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(self.inference_batch_size) {
            out.append(&mut self.sparse_one_batch(chunk)?);
        }
        Ok(out)
    }

    fn sparse_one_batch(&self, texts: &[&str]) -> Result<Vec<Option<SparseVector>>> {
        let (Some(sparse_linear), Backbone::Bgem3(xlm)) = (&self.sparse_linear, &self.backbone)
        else {
            return Ok(vec![None; texts.len()]);
        };
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
            return Ok(vec![None; batch_size]);
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
        let input_ids_t = Tensor::from_vec(input_ids.clone(), shape, &self.device)
            .map_err(|e| PgmcpError::Embedding(format!("input_ids tensor: {}", e)))?;
        let attention_mask_t = Tensor::from_vec(attention_mask.clone(), shape, &self.device)
            .map_err(|e| PgmcpError::Embedding(format!("attention_mask tensor: {}", e)))?;
        let token_type_ids_t = Tensor::from_vec(token_type_ids, shape, &self.device)
            .map_err(|e| PgmcpError::Embedding(format!("token_type_ids tensor: {}", e)))?;

        let hidden = xlm
            .forward(
                &input_ids_t,
                &attention_mask_t,
                &token_type_ids_t,
                None,
                None,
                None,
            )
            .map_err(|e| PgmcpError::Embedding(format!("XLM-RoBERTa forward (sparse): {}", e)))?;
        // (b, s, h) → (b, s, 1) → relu → (b, s), in F32 for the scatter-max.
        let weights = sparse_linear
            .forward(&hidden)
            .and_then(|t| t.relu())
            .and_then(|t| t.squeeze(2))
            .and_then(|t| t.to_dtype(DType::F32))
            .map_err(|e| PgmcpError::Embedding(format!("sparse projection: {}", e)))?;
        let w: Vec<Vec<f32>> = weights
            .to_vec2::<f32>()
            .map_err(|e| PgmcpError::Embedding(format!("sparse to_vec2: {}", e)))?;

        let mut result: Vec<Option<SparseVector>> = Vec::with_capacity(batch_size);
        for (r, row) in w.iter().enumerate() {
            // scatter-max: per vocab token, keep the maximum ReLU weight.
            let mut map: std::collections::BTreeMap<i32, f32> = std::collections::BTreeMap::new();
            for (t, &weight) in row.iter().enumerate().take(seq_len) {
                let idx = r * seq_len + t;
                if attention_mask[idx] == 0 {
                    continue; // padding
                }
                let id = input_ids[idx];
                if (0..=3).contains(&id) {
                    continue; // XLM-R special tokens
                }
                if weight <= 0.0 {
                    continue;
                }
                let slot = map.entry(id as i32).or_insert(0.0);
                if weight > *slot {
                    *slot = weight;
                }
            }
            // Always emit a vector (empty when no salient tokens) so the
            // backfill marks the chunk done instead of re-scanning forever.
            result.push(Some(SparseVector::from_map(
                map.iter(),
                self.sparse_dim as i32,
            )));
        }
        Ok(result)
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
                // Cast BF16 → F32 here so the subsequent L2 normalize
                // (and the downstream `to_vec2::<f32>` extraction)
                // runs in F32. Normalization in BF16 would accumulate
                // ~1e-3 noise in the per-row norm; the cast cost is
                // (batch × 1024) elements and dominated by the
                // forward pass.
                let pooled_bf16 = cls_pool(&hidden)
                    .map_err(|e| PgmcpError::Embedding(format!("cls_pool: {}", e)))?;
                pooled_bf16
                    .to_dtype(DType::F32)
                    .map_err(|e| PgmcpError::Embedding(format!("bf16→f32 cast: {}", e)))?
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
            // BGE-M3 ships its weights as `pytorch_model.bin` (no top-level
            // `model.safetensors` exists in the BAAI/bge-m3 HF repo at any
            // revision — verified via the HF API). The loader branches on
            // ModelKind to use `VarBuilder::from_pth` for this file.
            Self::Bgem3 => &["pytorch_model.bin", "config.json", "tokenizer.json"],
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

/// ColBERT late-interaction score (Khattab & Zaharia 2020): sum over query
/// tokens of the maximum cosine similarity to any document token. Inputs are
/// L2-normalized per token (so dot product == cosine). Higher is better; 0 for
/// an empty side. (graph-roadmap Phase 2.5)
pub fn colbert_maxsim(query: &[Vec<f32>], doc: &[Vec<f32>]) -> f32 {
    if query.is_empty() || doc.is_empty() {
        return 0.0;
    }
    let mut total = 0.0_f32;
    for q in query {
        let mut best = f32::NEG_INFINITY;
        for d in doc {
            let dot: f32 = q.iter().zip(d).map(|(a, b)| a * b).sum();
            if dot > best {
                best = dot;
            }
        }
        if best.is_finite() {
            total += best;
        }
    }
    total
}

// Re-export the `IndexOp` trait so the `i((..., 0, ...))` call above
// resolves. Adding the import at the top of the file is the conventional
// place, but keeping it scoped here documents *why* we need it.
use candle_core::IndexOp;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colbert_maxsim_scores_alignment() {
        // Query tokens that each align exactly with a doc token score ~= n_query.
        let q = vec![vec![1.0, 0.0], vec![0.0, 1.0]];
        let doc_match = vec![vec![1.0, 0.0], vec![0.0, 1.0], vec![0.5, 0.5]];
        let s_match = colbert_maxsim(&q, &doc_match);
        assert!(
            (s_match - 2.0).abs() < 1e-6,
            "perfect per-token match = 2, got {s_match}"
        );
        // An orthogonal doc scores lower.
        let doc_orth = vec![vec![0.0, 1.0]];
        let s_orth = colbert_maxsim(&[vec![1.0, 0.0]], &doc_orth);
        assert!(s_orth.abs() < 1e-6, "orthogonal = 0, got {s_orth}");
        // Empty sides score 0.
        assert_eq!(colbert_maxsim(&[], &doc_match), 0.0);
        assert_eq!(colbert_maxsim(&q, &[]), 0.0);
    }

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
    fn bgem3_model_files_targets_pytorch_bin() {
        // BAAI/bge-m3 HF repo ships pytorch_model.bin, not model.safetensors.
        // Requesting model.safetensors yields a 404 (deterministic, not
        // transient) — see plan
        // ~/.claude/plans/pgmcp-is-already-partially-glittery-graham.md F1.
        let files = ModelKind::Bgem3.model_files();
        assert!(
            files.contains(&"pytorch_model.bin"),
            "BGE-M3 file list must include pytorch_model.bin (got {:?})",
            files
        );
        assert!(
            !files.contains(&"model.safetensors"),
            "BGE-M3 HF repo does not publish model.safetensors (got {:?})",
            files
        );
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
