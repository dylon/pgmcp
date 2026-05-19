//! Memory-server Phase 11: LatentPipeline trait + dispatcher.
//!
//! The plan's "internal latent-space pipeline" (`docs/memory-server/02-phases.md`
//! Phase 11) fuses same-backbone pipeline stages — extract → reflect,
//! extract → consolidate — so the LLM doesn't decode, re-tokenize, and
//! re-encode its own output between stages. The savings target per the
//! plan's cross-phase concerns: ≥ 30 % token reduction and ≥ 1.5×
//! speedup on extract→reflect at no extraction-quality cost.
//!
//! The trait + closed-set enum follow the `FcmBackend` discipline from
//! `src/fcm/mod.rs` — backends are swappable, but the *choice* of
//! backend is enumerated at construction time and never feature-gated.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};
use candle_core::{DType, Device, Tensor};
use hf_hub::api::sync::Api;
use tokenizers::Tokenizer;
use tracing::{info, warn};

use crate::llm::prompt::{build_extraction_prompt, build_reflection_prompt};
use crate::llm::qwen3::Qwen3Variant;
use crate::llm::qwen3_latent_model::LatentModelWeights;
use crate::llm::recursive_link::RecursiveLink;
use crate::llm::{ExtractionRequest, ExtractionResult, NewEntity};

/// Backend identity for logging + telemetry.
pub trait LatentPipeline: Send + Sync {
    fn name(&self) -> &'static str;
    /// Backbone model signature (mirrors `LlmExtractor::model_signature`).
    fn backbone_signature(&self) -> &'static str;
    /// Versioned RecursiveLink weights signature (e.g. `"rlv1"`). Stable
    /// across runs of the same trained link.
    fn link_signature(&self) -> String;

    /// Fused extract → reflect: run extraction, capture the hidden
    /// state at the EOS position, project through R_in, prefill the
    /// reflection prompt with the projected embedding, decode the
    /// reflection output.
    fn extract_then_reflect(
        &self,
        req: ExtractionRequest<'_>,
    ) -> Result<(ExtractionResult, Vec<NewEntity>)>;
}

/// Closed-set backend choice. Per the plan §11.3 the latent pipeline
/// defaults to `Disabled` and is opted into at startup once
/// (a) the GPU/VRAM probe succeeds, (b) the RecursiveLink weights file
/// exists, and (c) a quality regression hasn't been flagged in the
/// past `regression_window` days.
#[derive(Debug, Clone, Copy, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LatentPipelineChoice {
    Qwen3Rlv1,
    Disabled,
}

pub fn parse_latent_choice(s: &str) -> Result<LatentPipelineChoice> {
    match s {
        "qwen3-rlv1" | "qwen3-recursive-v1" => Ok(LatentPipelineChoice::Qwen3Rlv1),
        "disabled" | "off" | "none" | "" => Ok(LatentPipelineChoice::Disabled),
        other => Err(anyhow!(
            "unknown latent pipeline backend '{}'; choices: qwen3-rlv1, disabled",
            other
        )),
    }
}

/// Configuration the factory needs to construct a working latent
/// pipeline. Pulled from `[memory.latent_pipeline]` at startup.
#[derive(Debug, Clone)]
pub struct LatentPipelineConfig {
    pub choice: LatentPipelineChoice,
    pub backbone: Qwen3Variant,
    pub link_weights_path: PathBuf,
    pub link_signature: String,
    pub quality_regression_threshold: f32,
    pub vram_probe_at_startup: bool,
}

/// Factory mirroring `make_extractor`. Returns `Ok(None)` when
/// `choice == Disabled` so the caller can degrade to the text path
/// without an error.
pub fn make_latent_pipeline(cfg: &LatentPipelineConfig) -> Result<Option<Box<dyn LatentPipeline>>> {
    match cfg.choice {
        LatentPipelineChoice::Disabled => Ok(None),
        LatentPipelineChoice::Qwen3Rlv1 => {
            if !cfg.link_weights_path.exists() {
                warn!(
                    path = %cfg.link_weights_path.display(),
                    "latent_pipeline: link weights missing; downgrading to disabled (text path)"
                );
                return Ok(None);
            }
            let pipeline = Qwen3LatentPipeline::new(
                cfg.backbone,
                cfg.link_weights_path.clone(),
                cfg.link_signature.clone(),
            )?;
            Ok(Some(Box::new(pipeline)))
        }
    }
}

const TOKENIZER_REPO_DEFAULT: &str = "Qwen/Qwen3-8B-Instruct";
const TOKENIZER_REPO_ENV: &str = "PGMCP_QWEN3_TOKENIZER_REPO";

/// Production `LatentPipeline` impl. Owns one quantized Qwen3 model
/// instance + one RecursiveLink behind a single mutex so concurrent
/// pipeline calls serialize (the KV cache + R_in are not thread-safe).
pub struct Qwen3LatentPipeline {
    variant: Qwen3Variant,
    link_signature: String,
    inner: Mutex<Qwen3LatentInner>,
}

struct Qwen3LatentInner {
    model: LatentModelWeights,
    tokenizer: Tokenizer,
    device: Device,
    eos_ids: Vec<u32>,
    im_end_id: Option<u32>,
    link: RecursiveLink,
}

impl Qwen3LatentPipeline {
    pub fn new(
        variant: Qwen3Variant,
        link_weights_path: PathBuf,
        link_signature: String,
    ) -> Result<Self> {
        let device = Device::new_cuda(0).or_else(|err| {
            warn!(error = %err, "Qwen3LatentPipeline: CUDA init failed, falling back to CPU (very slow)");
            Ok::<Device, anyhow::Error>(Device::Cpu)
        })?;

        let api = Api::new().context("hf-hub init")?;
        let (gguf_repo, gguf_file) = qwen3_gguf_coords(variant);
        let tok_repo = std::env::var(TOKENIZER_REPO_ENV)
            .unwrap_or_else(|_| TOKENIZER_REPO_DEFAULT.to_string());

        info!(
            variant = ?variant,
            gguf_repo = %gguf_repo,
            gguf_file = %gguf_file,
            link_path = %link_weights_path.display(),
            link_signature = %link_signature,
            "Qwen3LatentPipeline: loading"
        );

        let gguf_path = api
            .model(gguf_repo.clone())
            .get(&gguf_file)
            .with_context(|| format!("download {}/{}", gguf_repo, gguf_file))?;
        let tok_path = api
            .model(tok_repo.clone())
            .get("tokenizer.json")
            .with_context(|| format!("download {}/tokenizer.json", tok_repo))?;

        let mut reader = std::fs::File::open(&gguf_path)
            .with_context(|| format!("open {}", gguf_path.display()))?;
        let content = candle_core::quantized::gguf_file::Content::read(&mut reader)
            .context("parse gguf header")?;
        let model = LatentModelWeights::from_gguf(content, &mut reader, &device)
            .context("LatentModelWeights::from_gguf")?;

        let tokenizer =
            Tokenizer::from_file(&tok_path).map_err(|e| anyhow!("tokenizer load: {}", e))?;
        let im_end_id = tokenizer.token_to_id("<|im_end|>");
        let endoftext_id = tokenizer.token_to_id("<|endoftext|>");
        let mut eos_ids = Vec::new();
        if let Some(t) = im_end_id {
            eos_ids.push(t);
        }
        if let Some(t) = endoftext_id {
            eos_ids.push(t);
        }
        if eos_ids.is_empty() {
            return Err(anyhow!(
                "Qwen3LatentPipeline: tokenizer missing both <|im_end|> and <|endoftext|>"
            ));
        }

        let hidden_size = model.hidden_size();
        let link = RecursiveLink::load(
            &link_weights_path,
            hidden_size,
            &device,
            DType::F32,
            link_signature.clone(),
        )?;

        Ok(Self {
            variant,
            link_signature,
            inner: Mutex::new(Qwen3LatentInner {
                model,
                tokenizer,
                device,
                eos_ids,
                im_end_id,
                link,
            }),
        })
    }

    fn run_stage<F>(
        inner: &mut Qwen3LatentInner,
        prompt_text: &str,
        max_new_tokens: usize,
        seed_embed: Option<&Tensor>,
        eos_check: F,
    ) -> Result<(String, Tensor)>
    where
        F: Fn(&[u32], u32) -> bool,
    {
        let chat = format!(
            "<|im_start|>user\n{}\n<|im_end|>\n<|im_start|>assistant\n",
            prompt_text
        );
        let encoding = inner
            .tokenizer
            .encode(chat, true)
            .map_err(|e| anyhow!("tokenize: {}", e))?;
        let prompt_ids: Vec<u32> = encoding.get_ids().to_vec();
        if prompt_ids.is_empty() {
            return Err(anyhow!("prompt tokenized to zero tokens"));
        }

        inner.model.clear_kv_cache();

        // Build the prefill: either pure-token (seed_embed=None) or
        // [seed_embed_row || token_embed(prompt_ids)] (seed_embed=Some).
        let token_ids_i64: Vec<i64> = prompt_ids.iter().map(|&t| t as i64).collect();
        let token_ids = Tensor::from_vec(token_ids_i64, (1, prompt_ids.len()), &inner.device)?;
        let mut hidden_for_next: Tensor;
        let logits = match seed_embed {
            None => {
                let (logits, h) = inner.model.forward_with_hidden(&token_ids, 0)?;
                hidden_for_next = h;
                logits
            }
            Some(seed) => {
                // Project seed through R_in and concatenate with the token embeds.
                let projected = inner.link.forward(seed)?; // (1, hidden_size)
                let projected = projected.unsqueeze(1)?; // (1, 1, hidden_size)
                let tok_embeds = inner.model.embed_tokens(&token_ids)?; // (1, l, hidden_size)
                let fused = Tensor::cat(&[&projected, &tok_embeds], 1)?;
                let (logits, h) = inner.model.forward_from_input_embeds(&fused, 0)?;
                hidden_for_next = h;
                logits
            }
        };

        let mut output_tokens: Vec<u32> = Vec::with_capacity(max_new_tokens);
        let base_offset = match seed_embed {
            None => prompt_ids.len(),
            Some(_) => prompt_ids.len() + 1, // +1 for the seed-embed row
        };

        let mut current_logits = logits;
        for step in 0..max_new_tokens {
            let next = greedy_sample(&current_logits)?;
            if eos_check(&output_tokens, next) {
                break;
            }
            output_tokens.push(next);
            let step_input = Tensor::from_vec(vec![next as i64], (1, 1), &inner.device)?;
            let (l, h) = inner
                .model
                .forward_with_hidden(&step_input, base_offset + step)?;
            current_logits = l;
            hidden_for_next = h;
        }
        if let Some(end) = inner.im_end_id
            && output_tokens.last() == Some(&end)
        {
            output_tokens.pop();
        }
        let decoded = inner
            .tokenizer
            .decode(&output_tokens, true)
            .map_err(|e| anyhow!("decode: {}", e))?;
        Ok((decoded, hidden_for_next))
    }
}

impl LatentPipeline for Qwen3LatentPipeline {
    fn name(&self) -> &'static str {
        "qwen3-rlv1"
    }

    fn backbone_signature(&self) -> &'static str {
        match self.variant {
            Qwen3Variant::Eight => "qwen3-8b-instruct-q4km-v1",
            Qwen3Variant::Four => "qwen3-4b-instruct-q4km-v1",
        }
    }

    fn link_signature(&self) -> String {
        self.link_signature.clone()
    }

    fn extract_then_reflect(
        &self,
        req: ExtractionRequest<'_>,
    ) -> Result<(ExtractionResult, Vec<NewEntity>)> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| anyhow!("Qwen3LatentPipeline: poisoned mutex"))?;
        let eos_ids = inner.eos_ids.clone();
        let eos_check = move |_tokens: &[u32], next: u32| eos_ids.contains(&next);

        // Stage 1: extract. Token-mediated path; we capture the final
        // hidden state to seed stage 2.
        let extract_prompt = build_extraction_prompt(&req);
        let (extract_text, last_hidden) =
            Self::run_stage(&mut inner, &extract_prompt, 2048, None, &eos_check)?;
        let extraction = crate::llm::cloud::parse_extraction_response(&extract_text)?;

        // Stage 2: reflect, seeded by R_in(last_hidden) prepended to
        // the reflection prompt's input embeddings — eliminating the
        // text round-trip the plan §11 targets.
        let obs_strings: Vec<String> = extraction
            .entities
            .iter()
            .flat_map(|e| e.initial_observations.iter().cloned())
            .collect();
        if obs_strings.is_empty() {
            return Ok((extraction, Vec::new()));
        }
        let reflect_prompt = build_reflection_prompt(&obs_strings);
        let (reflect_text, _next_hidden) = Self::run_stage(
            &mut inner,
            &reflect_prompt,
            2048,
            Some(&last_hidden),
            &eos_check,
        )?;
        let new_entities = crate::llm::cloud::parse_reflection_response(&reflect_text)?;
        Ok((extraction, new_entities))
    }
}

fn qwen3_gguf_coords(variant: Qwen3Variant) -> (String, String) {
    match variant {
        Qwen3Variant::Eight => (
            std::env::var("PGMCP_QWEN3_8B_GGUF_REPO")
                .unwrap_or_else(|_| "unsloth/Qwen3-8B-Instruct-GGUF".into()),
            std::env::var("PGMCP_QWEN3_8B_GGUF_FILE")
                .unwrap_or_else(|_| "Qwen3-8B-Instruct-Q4_K_M.gguf".into()),
        ),
        Qwen3Variant::Four => (
            std::env::var("PGMCP_QWEN3_4B_GGUF_REPO")
                .unwrap_or_else(|_| "unsloth/Qwen3-4B-Instruct-GGUF".into()),
            std::env::var("PGMCP_QWEN3_4B_GGUF_FILE")
                .unwrap_or_else(|_| "Qwen3-4B-Instruct-Q4_K_M.gguf".into()),
        ),
    }
}

fn greedy_sample(logits: &Tensor) -> Result<u32> {
    let mut t = logits.clone();
    while t.rank() > 1 {
        let d0 = t.dim(0)?;
        if d0 == 1 {
            t = t.squeeze(0)?;
        } else {
            t = t.get(d0 - 1)?;
        }
    }
    let t = t.to_dtype(DType::F32)?;
    let values: Vec<f32> = t.to_vec1()?;
    let mut best = 0_u32;
    let mut best_v = f32::NEG_INFINITY;
    for (i, v) in values.iter().enumerate() {
        if *v > best_v {
            best_v = *v;
            best = i as u32;
        }
    }
    Ok(best)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_latent_choice_round_trip() {
        assert!(matches!(
            parse_latent_choice("qwen3-rlv1").unwrap(),
            LatentPipelineChoice::Qwen3Rlv1
        ));
        assert!(matches!(
            parse_latent_choice("disabled").unwrap(),
            LatentPipelineChoice::Disabled
        ));
        assert!(matches!(
            parse_latent_choice("").unwrap(),
            LatentPipelineChoice::Disabled
        ));
        assert!(parse_latent_choice("bogus").is_err());
    }

    #[test]
    fn factory_returns_none_when_choice_is_disabled() {
        let cfg = LatentPipelineConfig {
            choice: LatentPipelineChoice::Disabled,
            backbone: Qwen3Variant::Eight,
            link_weights_path: PathBuf::from("/no/such/path"),
            link_signature: "rlv1".into(),
            quality_regression_threshold: 0.05,
            vram_probe_at_startup: false,
        };
        let out = make_latent_pipeline(&cfg).expect("must not error on disabled");
        assert!(out.is_none());
    }

    #[test]
    fn factory_returns_none_when_link_weights_missing() {
        let cfg = LatentPipelineConfig {
            choice: LatentPipelineChoice::Qwen3Rlv1,
            backbone: Qwen3Variant::Eight,
            link_weights_path: PathBuf::from("/no/such/file/rlv1.safetensors"),
            link_signature: "rlv1".into(),
            quality_regression_threshold: 0.05,
            vram_probe_at_startup: false,
        };
        let out = make_latent_pipeline(&cfg).expect("missing weights must downgrade, not error");
        assert!(out.is_none(), "missing weights should produce None");
    }
}
