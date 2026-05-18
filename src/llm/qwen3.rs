//! Memory-server Phase 4: Qwen3 local `LlmExtractor` backend.
//!
//! Loads a Qwen3-Instruct GGUF Q4_K_M model via candle-transformers'
//! `quantized_qwen3` path, runs deterministic (greedy) generation, and
//! parses the structured JSON response.
//!
//! Model paths default to:
//! - 8B: `unsloth/Qwen3-8B-Instruct-GGUF` / `Qwen3-8B-Instruct-Q4_K_M.gguf`
//! - 4B: `unsloth/Qwen3-4B-Instruct-GGUF` / `Qwen3-4B-Instruct-Q4_K_M.gguf`
//!
//! Override via env (matched at construct time):
//! - `PGMCP_QWEN3_8B_GGUF_REPO`, `PGMCP_QWEN3_8B_GGUF_FILE`
//! - `PGMCP_QWEN3_4B_GGUF_REPO`, `PGMCP_QWEN3_4B_GGUF_FILE`
//! - `PGMCP_QWEN3_TOKENIZER_REPO` (shared)

use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow};
use candle_core::quantized::gguf_file;
use candle_core::{DType, Device, Tensor};
use candle_transformers::models::quantized_qwen3::ModelWeights;
use hf_hub::api::sync::Api;
use tokenizers::Tokenizer;
use tracing::{debug, info};

use crate::llm::prompt::{build_extraction_prompt, build_reflection_prompt};
use crate::llm::{ExtractionRequest, ExtractionResult, LlmExtractor, NewEntity};

/// Which variant to load. The 4B model is the fallback for tighter
/// VRAM configurations per `docs/memory-server/04-hardware.md`.
#[derive(Debug, Clone, Copy)]
pub enum Qwen3Variant {
    Eight,
    Four,
}

impl Qwen3Variant {
    fn default_gguf_repo(self) -> &'static str {
        match self {
            Self::Eight => "unsloth/Qwen3-8B-Instruct-GGUF",
            Self::Four => "unsloth/Qwen3-4B-Instruct-GGUF",
        }
    }
    fn default_gguf_file(self) -> &'static str {
        match self {
            Self::Eight => "Qwen3-8B-Instruct-Q4_K_M.gguf",
            Self::Four => "Qwen3-4B-Instruct-Q4_K_M.gguf",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Eight => "qwen3-8b-instruct-q4km",
            Self::Four => "qwen3-4b-instruct-q4km",
        }
    }
    fn signature(self) -> &'static str {
        match self {
            Self::Eight => "qwen3-8b-instruct-q4km-v1",
            Self::Four => "qwen3-4b-instruct-q4km-v1",
        }
    }
    fn gguf_repo_env(self) -> &'static str {
        match self {
            Self::Eight => "PGMCP_QWEN3_8B_GGUF_REPO",
            Self::Four => "PGMCP_QWEN3_4B_GGUF_REPO",
        }
    }
    fn gguf_file_env(self) -> &'static str {
        match self {
            Self::Eight => "PGMCP_QWEN3_8B_GGUF_FILE",
            Self::Four => "PGMCP_QWEN3_4B_GGUF_FILE",
        }
    }
}

const TOKENIZER_REPO_DEFAULT: &str = "Qwen/Qwen3-8B-Instruct";
const TOKENIZER_REPO_ENV: &str = "PGMCP_QWEN3_TOKENIZER_REPO";

/// Local Qwen3 extractor. Single-instance per process; serializes
/// access through a `Mutex` so concurrent extraction calls don't race
/// the model's KV cache.
pub struct Qwen3LocalExtractor {
    variant: Qwen3Variant,
    inner: Mutex<Qwen3Inner>,
}

struct Qwen3Inner {
    model: ModelWeights,
    tokenizer: Tokenizer,
    device: Device,
    eos_token_ids: Vec<u32>,
    im_end_token_id: Option<u32>,
}

impl Qwen3LocalExtractor {
    pub fn new(variant: Qwen3Variant) -> Result<Self> {
        let device = Device::new_cuda(0).or_else(|cuda_err| {
            tracing::warn!(error = %cuda_err, "Qwen3LocalExtractor: CUDA init failed, falling back to CPU (very slow)");
            Ok::<Device, anyhow::Error>(Device::Cpu)
        })?;

        let gguf_repo = std::env::var(variant.gguf_repo_env())
            .unwrap_or_else(|_| variant.default_gguf_repo().to_string());
        let gguf_file = std::env::var(variant.gguf_file_env())
            .unwrap_or_else(|_| variant.default_gguf_file().to_string());
        let tokenizer_repo = std::env::var(TOKENIZER_REPO_ENV)
            .unwrap_or_else(|_| TOKENIZER_REPO_DEFAULT.to_string());

        info!(
            variant = variant.label(),
            gguf_repo = %gguf_repo,
            gguf_file = %gguf_file,
            tokenizer_repo = %tokenizer_repo,
            "Qwen3LocalExtractor: loading model",
        );

        let api = Api::new().context("hf-hub init")?;
        let gguf_path: PathBuf = api
            .model(gguf_repo.clone())
            .get(&gguf_file)
            .with_context(|| format!("download {}/{}", gguf_repo, gguf_file))?;
        let tok_path: PathBuf = api
            .model(tokenizer_repo.clone())
            .get("tokenizer.json")
            .with_context(|| format!("download {}/tokenizer.json", tokenizer_repo))?;

        let mut gguf_reader =
            File::open(&gguf_path).with_context(|| format!("open gguf {}", gguf_path.display()))?;
        let content = gguf_file::Content::read(&mut gguf_reader).context("parse gguf header")?;
        let model = ModelWeights::from_gguf(content, &mut gguf_reader, &device)
            .context("ModelWeights::from_gguf")?;

        let tokenizer =
            Tokenizer::from_file(&tok_path).map_err(|e| anyhow!("tokenizer load: {}", e))?;

        // Resolve EOS-equivalent token ids. Qwen3 uses `<|im_end|>` for
        // chat-turn termination plus `<|endoftext|>` for hard EOS.
        let im_end_token_id = tokenizer.token_to_id("<|im_end|>");
        let endoftext_token_id = tokenizer.token_to_id("<|endoftext|>");
        let mut eos_token_ids = Vec::new();
        if let Some(t) = im_end_token_id {
            eos_token_ids.push(t);
        }
        if let Some(t) = endoftext_token_id {
            eos_token_ids.push(t);
        }
        if eos_token_ids.is_empty() {
            return Err(anyhow!(
                "Qwen3 tokenizer missing both <|im_end|> and <|endoftext|>; refusing to load — \
                 generation would never terminate"
            ));
        }

        Ok(Self {
            variant,
            inner: Mutex::new(Qwen3Inner {
                model,
                tokenizer,
                device,
                eos_token_ids,
                im_end_token_id,
            }),
        })
    }

    fn generate(&self, prompt: &str, max_new_tokens: usize) -> Result<String> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| anyhow!("Qwen3LocalExtractor: poisoned mutex"))?;
        let chat = format_chat_prompt(prompt);
        let encoding = inner
            .tokenizer
            .encode(chat, true)
            .map_err(|e| anyhow!("tokenize: {}", e))?;
        let prompt_ids: Vec<u32> = encoding.get_ids().to_vec();
        if prompt_ids.is_empty() {
            return Err(anyhow!("prompt tokenized to zero tokens"));
        }

        inner.model.clear_kv_cache();
        let input_ids_i64: Vec<i64> = prompt_ids.iter().map(|&t| t as i64).collect();
        let input = Tensor::from_vec(input_ids_i64, (1, prompt_ids.len()), &inner.device)?;
        let mut logits = inner.model.forward(&input, 0)?;

        let mut output_tokens: Vec<u32> = Vec::with_capacity(max_new_tokens);
        let base_offset = prompt_ids.len();
        for step in 0..max_new_tokens {
            let next = greedy_sample(&logits)?;
            if inner.eos_token_ids.contains(&next) {
                break;
            }
            output_tokens.push(next);
            let step_input = Tensor::from_vec(vec![next as i64], (1, 1), &inner.device)?;
            logits = inner.model.forward(&step_input, base_offset + step)?;
        }
        // Drop a trailing `<|im_end|>` if the loop captured it via EOS detection
        // on the next-step prediction (defensive — current loop already breaks
        // before pushing the EOS, but the check is cheap).
        if let Some(end) = inner.im_end_token_id
            && output_tokens.last() == Some(&end)
        {
            output_tokens.pop();
        }
        let decoded = inner
            .tokenizer
            .decode(&output_tokens, true)
            .map_err(|e| anyhow!("decode: {}", e))?;
        debug!(
            variant = self.variant.label(),
            prompt_tokens = prompt_ids.len(),
            new_tokens = output_tokens.len(),
            "Qwen3LocalExtractor: completion",
        );
        Ok(decoded)
    }
}

impl LlmExtractor for Qwen3LocalExtractor {
    fn name(&self) -> &'static str {
        self.variant.label()
    }

    fn model_signature(&self) -> &'static str {
        signature_for(self.variant.signature())
    }

    fn extract(&self, request: ExtractionRequest<'_>) -> Result<ExtractionResult> {
        let prompt = build_extraction_prompt(&request);
        let raw = self.generate(&prompt, 2048)?;
        crate::llm::cloud::parse_extraction_response(&raw)
    }

    fn reflect(&self, observations: &[String]) -> Result<Vec<NewEntity>> {
        if observations.is_empty() {
            return Ok(Vec::new());
        }
        let prompt = build_reflection_prompt(observations);
        let raw = self.generate(&prompt, 2048)?;
        crate::llm::cloud::parse_reflection_response(&raw)
    }
}

/// Greedy argmax over the model's last-position logits. Logits arrive
/// as a `(1, 1, vocab_size)` (or similar) tensor — we squeeze and
/// scan. f32 cast so f16 quant weights round to a stable next-token.
fn greedy_sample(logits: &Tensor) -> Result<u32> {
    // Reduce to a 1-D vocab tensor regardless of incoming rank.
    let mut t = logits.clone();
    while t.rank() > 1 {
        let dim0 = t.dim(0)?;
        if dim0 == 1 {
            t = t.squeeze(0)?;
        } else {
            // Shouldn't happen for batch=1 inputs.
            t = t.get(dim0 - 1)?;
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

/// Wrap a user-turn prompt in Qwen3's chat template. We always set an
/// empty system message and supply the user content as the only turn;
/// the prompt body already contains the structured extraction
/// instructions.
fn format_chat_prompt(user: &str) -> String {
    format!(
        "<|im_start|>system\nYou are a structured-output assistant; respond only as instructed.<|im_end|>\n\
         <|im_start|>user\n{}\n<|im_end|>\n\
         <|im_start|>assistant\n",
        user
    )
}

fn signature_for(label: &'static str) -> &'static str {
    static SLOT: OnceLock<&'static str> = OnceLock::new();
    SLOT.get_or_init(|| label)
}

// File usage suppression — the `Read` import is used transitively by
// `gguf_file::Content::read` through its generic bound and the compiler
// otherwise warns on the `use std::io::Read` line.
#[allow(dead_code)]
fn _read_witness<R: Read>(_: R) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_chat_prompt_includes_required_tags() {
        let p = format_chat_prompt("hello");
        assert!(p.contains("<|im_start|>user"));
        assert!(p.contains("<|im_end|>"));
        assert!(p.ends_with("<|im_start|>assistant\n"));
    }

    #[test]
    fn variant_defaults_make_sense() {
        assert!(Qwen3Variant::Eight.default_gguf_file().contains("8B"));
        assert!(Qwen3Variant::Four.default_gguf_file().contains("4B"));
        assert_ne!(
            Qwen3Variant::Eight.signature(),
            Qwen3Variant::Four.signature()
        );
    }
}
