//! Memory-server Phase 7: `BGE-reranker-v2-m3` candle backend.
//!
//! Loads `BAAI/bge-reranker-v2-m3` (XLM-RoBERTa-base architecture with
//! a classification head) and exposes a single-score-per-(query,
//! candidate)-pair surface. Uses sequence-classification logits
//! (single-label) sigmoid as the relevance score.
//!
//! Memory footprint: ~600 MB at fp16. Loaded on demand by the
//! dispatcher; mutually exclusive in VRAM with the Qwen3 extractor.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::xlm_roberta::{Config, XLMRobertaForSequenceClassification};
use hf_hub::api::sync::Api;
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams, TruncationStrategy};

use super::{RerankHit, Reranker};

const HF_REPO: &str = "BAAI/bge-reranker-v2-m3";
const MODEL_FILES: &[&str] = &["model.safetensors", "config.json", "tokenizer.json"];

pub struct BgeRerankerV2M3 {
    inner: Mutex<Inner>,
}

struct Inner {
    model: XLMRobertaForSequenceClassification,
    tokenizer: Tokenizer,
    device: Device,
    max_length: usize,
}

impl BgeRerankerV2M3 {
    pub fn new() -> Result<Self> {
        let device = Device::new_cuda(0).or_else(|e| {
            tracing::error!(error = %e, "BgeRerankerV2M3: CUDA init failed; falling back to CPU");
            Ok::<Device, anyhow::Error>(Device::Cpu)
        })?;
        let api = Api::new().context("hf-hub init")?;
        let model_api = api.model(HF_REPO.to_string());
        let mut dir: Option<PathBuf> = None;
        for f in MODEL_FILES {
            let p = model_api
                .get(f)
                .with_context(|| format!("hf get {}/{}", HF_REPO, f))?;
            if dir.is_none() {
                dir = p.parent().map(Path::to_path_buf);
            }
        }
        let dir = dir.ok_or_else(|| anyhow!("hf cache resolve"))?;

        let cfg_path = dir.join("config.json");
        let cfg_json = std::fs::read_to_string(&cfg_path).context("read config.json")?;
        let cfg: Config = serde_json::from_str(&cfg_json).context("parse config.json")?;

        let weights_path = dir.join("model.safetensors");
        // SAFETY: HF cache file is read-only and stable for session lifetime.
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[weights_path], DType::F16, &device)
                .context("safetensors load")?
        };
        let model = XLMRobertaForSequenceClassification::new(1, &cfg, vb)
            .context("XLMRobertaForSequenceClassification::new")?;

        let tok_path = dir.join("tokenizer.json");
        let mut tokenizer =
            Tokenizer::from_file(&tok_path).map_err(|e| anyhow!("tokenizer load: {}", e))?;
        let max_length: usize = 512;
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
            .map_err(|e| anyhow!("tokenizer truncation: {}", e))?;

        Ok(Self {
            inner: Mutex::new(Inner {
                model,
                tokenizer,
                device,
                max_length,
            }),
        })
    }
}

impl Reranker for BgeRerankerV2M3 {
    fn name(&self) -> &'static str {
        "bge-reranker-v2-m3"
    }

    fn rerank(&self, query: &str, candidates: &[&str]) -> Result<Vec<RerankHit>> {
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let inner = self
            .inner
            .lock()
            .map_err(|_| anyhow!("BgeRerankerV2M3: poisoned mutex"))?;
        // Build the (query, candidate) pairs as the tokenizer's
        // pair-input. Cross-encoder semantics: query is sentence A,
        // candidate is sentence B.
        let mut inputs: Vec<(String, String)> = Vec::with_capacity(candidates.len());
        for c in candidates {
            inputs.push((query.to_string(), (*c).to_string()));
        }
        let encodings = inner
            .tokenizer
            .encode_batch(inputs, true)
            .map_err(|e| anyhow!("encode_batch: {}", e))?;
        let batch_size = encodings.len();
        let seq_len = encodings
            .iter()
            .map(|e| e.get_ids().len())
            .max()
            .unwrap_or(0)
            .min(inner.max_length);
        if seq_len == 0 {
            return Ok(candidates
                .iter()
                .enumerate()
                .map(|(i, _)| RerankHit {
                    original_index: i,
                    score: 0.0,
                })
                .collect());
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
        let input_ids_t =
            Tensor::from_vec(input_ids, shape, &inner.device).context("input_ids tensor")?;
        let attention_mask_t = Tensor::from_vec(attention_mask, shape, &inner.device)
            .context("attention_mask tensor")?;
        let token_type_ids_t = Tensor::from_vec(token_type_ids, shape, &inner.device)
            .context("token_type_ids tensor")?;

        let logits = inner
            .model
            .forward(&input_ids_t, &attention_mask_t, &token_type_ids_t)
            .context("classification forward")?;
        // Single-label classification head produces shape (batch, 1).
        // Sigmoid → relevance score in (0, 1).
        let scores_t = logits.to_dtype(DType::F32)?.squeeze(1)?;
        let raw_scores: Vec<f32> = scores_t.to_vec1::<f32>().context("logits → vec1")?;
        let mut hits: Vec<RerankHit> = raw_scores
            .into_iter()
            .enumerate()
            .map(|(i, l)| RerankHit {
                original_index: i,
                score: sigmoid(l),
            })
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(hits)
    }
}

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}
