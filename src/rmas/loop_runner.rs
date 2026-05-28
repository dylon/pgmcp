//! Tier-3 v1 homogeneous latent loop engine (ADR-009; RecursiveMAS §3–5).
//!
//! One resident Qwen3 backbone; each role has its own RecursiveLink (`R_in`) and
//! system prompt. Latent state passes role→role — each hop prefills the role's
//! prompt seeded by the prior role's hidden state projected through *this* role's
//! link — for `rounds` rounds (A1→…→A_N→A1); only the final round's last role
//! decodes to text (intermediate hops stay in latent space, `max_new_tokens = 0`).
//! All roles share the one backbone, so `W₃ = I` (the homogeneous case) and the
//! inner link `R_in` suffices.
//!
//! **Hardware-gated.** `load` requires CUDA + the backbone GGUF; on failure
//! `make_engine` returns `Ok(None)` and callers degrade to the Tier-2 text path
//! (the same posture as `make_latent_pipeline`). The self-contained backbone
//! load + decode loop mirror the Phase-11-proven `Qwen3LatentPipeline` (kept
//! untouched), parameterized by a per-role link.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};
use candle_core::{DType, Device, Tensor};
use hf_hub::api::sync::Api;
use tokenizers::Tokenizer;
use tracing::info;

use crate::llm::qwen3::Qwen3Variant;
use crate::llm::qwen3_latent_model::LatentModelWeights;
use crate::llm::recursive_link::RecursiveLink;
use crate::rmas::RmasEngine;
use crate::rmas::topology::RmasTopology;

const TOKENIZER_REPO_DEFAULT: &str = "Qwen/Qwen3-8B-Instruct";

pub struct HomogeneousQwen3Engine {
    topology: RmasTopology,
    backbone_sig: &'static str,
    inner: Mutex<Inner>,
}

struct Inner {
    model: LatentModelWeights,
    tokenizer: Tokenizer,
    device: Device,
    eos_ids: Vec<u32>,
    im_end_id: Option<u32>,
    links: Vec<RecursiveLink>, // index-aligned with topology.roles
}

impl HomogeneousQwen3Engine {
    /// Load the backbone + per-role links. Errors (no CUDA, missing GGUF) make
    /// `make_engine` degrade to `None`.
    pub fn load(backbone: Qwen3Variant, topology: RmasTopology, link_dir: &Path) -> Result<Self> {
        if topology.roles.is_empty() {
            return Err(anyhow!("RMAS topology has no roles"));
        }
        let device = Device::new_cuda(0).context("RMAS latent loop requires CUDA")?;

        // Pre-flight VRAM check before the expensive GGUF read: the homogeneous
        // loop keeps one resident backbone + N per-role links and must refuse to
        // load when that footprint won't fit (the 8 GB wall). A failure here
        // degrades the engine to None at the factory boundary.
        let n_links = topology.roles.len();
        let need = crate::rmas::residency::homogeneous_footprint_bytes(
            backbone,
            n_links,
            crate::rmas::residency::expected_hidden_size(backbone),
        );
        let budget = crate::rmas::residency::probe_cuda_vram()?;
        if !budget.fits(need, crate::rmas::residency::DEFAULT_HEADROOM_FRAC) {
            return Err(anyhow!(
                "insufficient VRAM for homogeneous loop: need ~{} MB resident + headroom, only {} MB free of {} MB",
                need >> 20,
                budget.free_bytes >> 20,
                budget.total_bytes >> 20
            ));
        }

        let (gguf_repo, gguf_file) = gguf_coords(backbone);
        let tok_repo = std::env::var("PGMCP_QWEN3_TOKENIZER_REPO")
            .unwrap_or_else(|_| TOKENIZER_REPO_DEFAULT.to_string());

        let api = Api::new().context("hf-hub init")?;
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
        let mut eos_ids = Vec::new();
        if let Some(t) = im_end_id {
            eos_ids.push(t);
        }
        if let Some(t) = tokenizer.token_to_id("<|endoftext|>") {
            eos_ids.push(t);
        }
        if eos_ids.is_empty() {
            return Err(anyhow!(
                "tokenizer missing both <|im_end|> and <|endoftext|>"
            ));
        }

        let hidden_size = model.hidden_size();
        // Per-role inner links: load the trained R_in if present, else a fresh
        // residual-identity (passthrough) so the loop runs before any training.
        let mut links = Vec::with_capacity(topology.roles.len());
        for spec in &topology.roles {
            let path = link_dir.join(format!("rin__{}.safetensors", sanitize(spec.role.as_str())));
            let link = if path.exists() {
                RecursiveLink::load(
                    &path,
                    hidden_size,
                    &device,
                    DType::F32,
                    spec.role.to_string(),
                )?
            } else {
                RecursiveLink::new_residual_identity(
                    hidden_size,
                    &device,
                    DType::F32,
                    spec.role.to_string(),
                )?
                .0
            };
            links.push(link);
        }

        let backbone_sig = match backbone {
            Qwen3Variant::Eight => "qwen3-8b-instruct-q4km-v1",
            Qwen3Variant::Four => "qwen3-4b-instruct-q4km-v1",
        };
        info!(
            roles = topology.roles.len(),
            rounds = topology.rounds,
            "rmas: homogeneous latent loop engine loaded"
        );
        Ok(Self {
            topology,
            backbone_sig,
            inner: Mutex::new(Inner {
                model,
                tokenizer,
                device,
                eos_ids,
                im_end_id,
                links,
            }),
        })
    }
}

impl RmasEngine for HomogeneousQwen3Engine {
    fn name(&self) -> &'static str {
        "homogeneous-qwen3"
    }

    fn backbone_signature(&self) -> &'static str {
        self.backbone_sig
    }

    fn run_loop(&self, query: &str, max_new_tokens: usize) -> Result<String> {
        let mut guard = self.inner.lock().map_err(|_| anyhow!("poisoned mutex"))?;
        let Inner {
            model,
            tokenizer,
            device,
            eos_ids,
            im_end_id,
            links,
        } = &mut *guard;

        let mut carried: Option<Tensor> = None;
        let mut final_text = String::new();
        for (round, role_idx) in self.topology.schedule() {
            let spec = &self.topology.roles[role_idx];
            let is_final = self.topology.is_final_hop(round, role_idx);
            let prompt = format!("{}\n\nQuery:\n{}", spec.system_prompt, query);
            // Intermediate hops stay latent (prefill only → hidden); the final
            // hop decodes the textual answer.
            let max_tok = if is_final { max_new_tokens } else { 0 };
            // Project the incoming hidden through *this* (receiving) role's inner
            // link before the hop. Same-width (`W₃ = I`), so the projection stays
            // in the shared latent space — the homogeneous case.
            let seed = match carried.as_ref() {
                Some(h) => Some(links[role_idx].forward(h)?),
                None => None,
            };
            let (text, hidden) = latent_hop(
                model,
                tokenizer,
                device,
                eos_ids,
                *im_end_id,
                &prompt,
                seed.as_ref(),
                max_tok,
            )?;
            carried = Some(hidden);
            if is_final {
                final_text = text;
            }
        }
        Ok(final_text)
    }
}

/// One latent hop — the proven `Qwen3LatentPipeline::run_stage` logic.
/// `seed_embed`, when present, must *already* be at this model's hidden width
/// (the caller applies the inner/outer link before the hop); it is prepended to
/// the prompt's token embeddings as a latent prefill row. `max_new_tokens == 0`
/// prefills only and returns the hidden state (a pure latent hop, no decode).
/// Shared by the homogeneous and heterogeneous engines.
#[allow(clippy::too_many_arguments)]
pub(crate) fn latent_hop(
    model: &mut LatentModelWeights,
    tokenizer: &Tokenizer,
    device: &Device,
    eos_ids: &[u32],
    im_end_id: Option<u32>,
    prompt_text: &str,
    seed_embed: Option<&Tensor>,
    max_new_tokens: usize,
) -> Result<(String, Tensor)> {
    let chat = format!(
        "<|im_start|>user\n{}\n<|im_end|>\n<|im_start|>assistant\n",
        prompt_text
    );
    let encoding = tokenizer
        .encode(chat, true)
        .map_err(|e| anyhow!("tokenize: {}", e))?;
    let prompt_ids: Vec<u32> = encoding.get_ids().to_vec();
    if prompt_ids.is_empty() {
        return Err(anyhow!("prompt tokenized to zero tokens"));
    }
    model.clear_kv_cache();
    let token_ids_i64: Vec<i64> = prompt_ids.iter().map(|&t| t as i64).collect();
    let token_ids = Tensor::from_vec(token_ids_i64, (1, prompt_ids.len()), device)?;

    let mut hidden_for_next: Tensor;
    let logits = match seed_embed {
        None => {
            let (logits, h) = model.forward_with_hidden(&token_ids, 0)?;
            hidden_for_next = h;
            logits
        }
        Some(seed) => {
            // Seed is already at this model's width (caller-projected); prepend it
            // as the latent prefill row.
            let projected = seed.unsqueeze(1)?; // (1,1,hidden)
            let tok_embeds = model.embed_tokens(&token_ids)?;
            let fused = Tensor::cat(&[&projected, &tok_embeds], 1)?;
            let (logits, h) = model.forward_from_input_embeds(&fused, 0)?;
            hidden_for_next = h;
            logits
        }
    };

    let base_offset = match seed_embed {
        None => prompt_ids.len(),
        Some(_) => prompt_ids.len() + 1,
    };
    let mut output_tokens: Vec<u32> = Vec::with_capacity(max_new_tokens);
    let mut current_logits = logits;
    for step in 0..max_new_tokens {
        let next = greedy_sample(&current_logits)?;
        if eos_ids.contains(&next) {
            break;
        }
        output_tokens.push(next);
        let step_input = Tensor::from_vec(vec![next as i64], (1, 1), device)?;
        let (l, h) = model.forward_with_hidden(&step_input, base_offset + step)?;
        current_logits = l;
        hidden_for_next = h;
    }
    if let Some(end) = im_end_id
        && output_tokens.last() == Some(&end)
    {
        output_tokens.pop();
    }
    let decoded = if output_tokens.is_empty() {
        String::new()
    } else {
        tokenizer
            .decode(&output_tokens, true)
            .map_err(|e| anyhow!("decode: {}", e))?
    };
    Ok((decoded, hidden_for_next))
}

pub(crate) fn greedy_sample(logits: &Tensor) -> Result<u32> {
    let mut t = logits.clone();
    while t.rank() > 1 {
        let d0 = t.dim(0)?;
        t = if d0 == 1 {
            t.squeeze(0)?
        } else {
            t.get(d0 - 1)?
        };
    }
    let values: Vec<f32> = t.to_dtype(DType::F32)?.to_vec1()?;
    let mut best = 0u32;
    let mut best_v = f32::NEG_INFINITY;
    for (i, v) in values.iter().enumerate() {
        if *v > best_v {
            best_v = *v;
            best = i as u32;
        }
    }
    Ok(best)
}

pub(crate) fn gguf_coords(variant: Qwen3Variant) -> (String, String) {
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

pub(crate) fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}
