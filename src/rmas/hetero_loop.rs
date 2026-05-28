//! Tier-3 v2 heterogeneous latent loop engine (ADR-009 R4; RecursiveMAS §3–5).
//!
//! The generalization of the homogeneous loop ([`crate::rmas::loop_runner`]) to
//! roles on *different* backbones. Each role names a [`Qwen3Variant`]; one
//! resident backbone is loaded per *distinct* variant. The hand-off between
//! consecutive roles crosses dimensions through an [`OuterLink`] (`W₃ :
//! d_src→d_tgt`, the cross-width residual) instead of the same-width inner link —
//! this is exactly what the outer link exists for. The loop is the ring
//! A₁→…→Aₙ→A₁ for `rounds` rounds; only the final round's last role decodes.
//!
//! **Hardware reality (plan risk 6).** A genuine cross-architecture loop needs
//! every distinct backbone co-resident: 4B (2560) + 8B (4096) ≈ 9.15 GB exceeds
//! the project's 8 GB card, so the residency gate refuses it locally and it runs
//! only on a bigger GPU / cloud. The engine is complete and correct; the
//! constraint is hardware, not design. `make_engine` returns `None` when the
//! footprint won't fit, degrading to the Tier-2 text path — the same posture as
//! the homogeneous engine and `make_latent_pipeline`.
//!
//! The decode primitive (`loop_runner::latent_hop`), GGUF coordinates, and
//! sanitizer are shared with the homogeneous engine; only the multi-backbone
//! residency + the outer-link cross-dim hops are new here.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};
use candle_core::{DType, Device, Tensor};
use hf_hub::api::sync::Api;
use tokenizers::Tokenizer;
use tracing::info;

use crate::csm::role::Role;
use crate::llm::qwen3::Qwen3Variant;
use crate::llm::qwen3_latent_model::LatentModelWeights;
use crate::rmas::RmasEngine;
use crate::rmas::loop_runner::{gguf_coords, latent_hop, sanitize};
use crate::rmas::outer_link::OuterLink;
use crate::rmas::residency;

const TOKENIZER_REPO_DEFAULT: &str = "Qwen/Qwen3-8B-Instruct";

/// One role in the heterogeneous loop, pinned to a specific backbone.
#[derive(Debug, Clone)]
pub struct HeteroRoleSpec {
    pub role: Role,
    pub system_prompt: String,
    pub backbone: Qwen3Variant,
}

/// Roles + round count for the heterogeneous ring.
#[derive(Debug, Clone)]
pub struct HeteroTopology {
    pub roles: Vec<HeteroRoleSpec>,
    pub rounds: usize,
}

impl HeteroTopology {
    pub fn new(roles: Vec<HeteroRoleSpec>, rounds: usize) -> Self {
        HeteroTopology {
            roles,
            rounds: rounds.max(1),
        }
    }

    pub fn n_roles(&self) -> usize {
        self.roles.len()
    }

    /// Distinct backbones, in first-appearance order (one resident model each).
    pub fn distinct_backbones(&self) -> Vec<Qwen3Variant> {
        let mut seen = Vec::new();
        for r in &self.roles {
            if !seen.contains(&r.backbone) {
                seen.push(r.backbone);
            }
        }
        seen
    }

    /// True when every role shares one backbone (the loop is *really*
    /// homogeneous and the simpler engine should be used instead).
    pub fn is_effectively_homogeneous(&self) -> bool {
        self.distinct_backbones().len() <= 1
    }

    /// The `(src_width, tgt_width)` of each ring edge `i → (i+1) mod n`.
    pub fn ring_edge_widths(&self) -> Vec<(usize, usize)> {
        let n = self.roles.len();
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let j = (i + 1) % n;
            out.push((
                residency::expected_hidden_size(self.roles[i].backbone),
                residency::expected_hidden_size(self.roles[j].backbone),
            ));
        }
        out
    }

    /// The ordered `(round, role_index)` hop schedule (final entry decodes).
    pub fn schedule(&self) -> Vec<(usize, usize)> {
        let mut s = Vec::with_capacity(self.rounds * self.roles.len());
        for round in 0..self.rounds {
            for i in 0..self.roles.len() {
                s.push((round, i));
            }
        }
        s
    }

    pub fn is_final_hop(&self, round: usize, role_idx: usize) -> bool {
        !self.roles.is_empty() && round + 1 == self.rounds && role_idx + 1 == self.roles.len()
    }
}

/// One resident backbone (model + tokenizer + EOS metadata).
struct ResidentBackbone {
    model: LatentModelWeights,
    tokenizer: Tokenizer,
    eos_ids: Vec<u32>,
    im_end_id: Option<u32>,
}

struct HeteroInner {
    device: Device,
    /// One per distinct variant.
    backbones: Vec<ResidentBackbone>,
    /// role index → backbones index.
    role_backbone_idx: Vec<usize>,
    /// One outer link per ring edge `i → (i+1) mod n`.
    edge_links: Vec<OuterLink>,
}

pub struct HeterogeneousQwen3Engine {
    topology: HeteroTopology,
    inner: Mutex<HeteroInner>,
}

impl HeterogeneousQwen3Engine {
    /// Load every distinct backbone + the per-edge outer links. Errors (no CUDA,
    /// insufficient VRAM, missing GGUF) make `make_engine` degrade to `None`.
    pub fn load(topology: HeteroTopology, link_dir: &Path) -> Result<Self> {
        if topology.roles.is_empty() {
            return Err(anyhow!("heterogeneous topology has no roles"));
        }
        let device = Device::new_cuda(0).context("RMAS heterogeneous loop requires CUDA")?;

        // VRAM pre-flight: every distinct backbone must be co-resident.
        let distinct = topology.distinct_backbones();
        let ring_widths = topology.ring_edge_widths();
        let need = residency::heterogeneous_footprint_bytes(&distinct, &ring_widths);
        let budget = residency::probe_cuda_vram()?;
        if !budget.fits(need, residency::DEFAULT_HEADROOM_FRAC) {
            return Err(anyhow!(
                "insufficient VRAM for heterogeneous loop: need ~{} MB resident ({} distinct backbones) + headroom, only {} MB free of {} MB",
                need >> 20,
                distinct.len(),
                budget.free_bytes >> 20,
                budget.total_bytes >> 20
            ));
        }

        // One resident backbone per distinct variant.
        let mut backbones = Vec::with_capacity(distinct.len());
        let mut variant_to_idx: HashMap<Qwen3Variant, usize> =
            HashMap::with_capacity(distinct.len());
        for (idx, variant) in distinct.iter().enumerate() {
            backbones.push(load_backbone(*variant, &device)?);
            variant_to_idx.insert(*variant, idx);
        }
        let role_backbone_idx: Vec<usize> = topology
            .roles
            .iter()
            .map(|r| variant_to_idx[&r.backbone])
            .collect();

        // One outer link per ring edge, cross-dim when the two roles' widths
        // differ. A trained link on disk (`rout__<src>__<tgt>.safetensors`) is
        // loaded; otherwise a fresh link (identity residual when widths match,
        // else a learned-from-init projection).
        let n = topology.roles.len();
        let mut edge_links = Vec::with_capacity(n);
        for (i, &(src, tgt)) in ring_widths.iter().enumerate() {
            let j = (i + 1) % n;
            let sig = format!(
                "rout-v1::{}->{}",
                topology.roles[i].role.as_str(),
                topology.roles[j].role.as_str()
            );
            let path = link_dir.join(format!(
                "rout__{}__{}.safetensors",
                sanitize(topology.roles[i].role.as_str()),
                sanitize(topology.roles[j].role.as_str())
            ));
            let link = if path.exists() {
                OuterLink::load(&path, src, tgt, &device, DType::F32, sig)?
            } else {
                OuterLink::new(src, tgt, &device, DType::F32, sig)?.0
            };
            edge_links.push(link);
        }

        info!(
            roles = topology.roles.len(),
            backbones = distinct.len(),
            rounds = topology.rounds,
            "rmas: heterogeneous latent loop engine loaded"
        );
        Ok(Self {
            topology,
            inner: Mutex::new(HeteroInner {
                device,
                backbones,
                role_backbone_idx,
                edge_links,
            }),
        })
    }
}

impl RmasEngine for HeterogeneousQwen3Engine {
    fn name(&self) -> &'static str {
        "heterogeneous-qwen3"
    }

    fn backbone_signature(&self) -> &'static str {
        // A mixed-backbone loop has no single signature; the per-role backbones
        // live in the topology. This names the kind.
        "heterogeneous-qwen3-mixed"
    }

    fn run_loop(&self, query: &str, max_new_tokens: usize) -> Result<String> {
        let mut guard = self.inner.lock().map_err(|_| anyhow!("poisoned mutex"))?;
        let HeteroInner {
            device,
            backbones,
            role_backbone_idx,
            edge_links,
        } = &mut *guard;

        let mut carried: Option<Tensor> = None;
        let mut final_text = String::new();
        for (round, role_idx) in self.topology.schedule() {
            let spec = &self.topology.roles[role_idx];
            let is_final = self.topology.is_final_hop(round, role_idx);
            let prompt = format!("{}\n\nQuery:\n{}", spec.system_prompt, query);
            let max_tok = if is_final { max_new_tokens } else { 0 };

            // The carried hidden was already projected to *this* role's width by
            // the previous ring edge's outer link, so it can seed directly.
            let bidx = role_backbone_idx[role_idx];
            let bb = &mut backbones[bidx];
            let (text, hidden) = latent_hop(
                &mut bb.model,
                &bb.tokenizer,
                device,
                &bb.eos_ids,
                bb.im_end_id,
                &prompt,
                carried.as_ref(),
                max_tok,
            )?;

            if is_final {
                final_text = text;
            } else {
                // Project this role's output to the *next* role's width via the
                // ring edge link, ready to seed the next hop.
                carried = Some(edge_links[role_idx].forward(&hidden)?);
            }
        }
        Ok(final_text)
    }
}

/// Load one resident backbone (mirrors the homogeneous loader, kept self-contained
/// so the heterogeneous engine owns its own model lifecycle).
fn load_backbone(variant: Qwen3Variant, device: &Device) -> Result<ResidentBackbone> {
    let (gguf_repo, gguf_file) = gguf_coords(variant);
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

    let mut reader =
        std::fs::File::open(&gguf_path).with_context(|| format!("open {}", gguf_path.display()))?;
    let content = candle_core::quantized::gguf_file::Content::read(&mut reader)
        .context("parse gguf header")?;
    let model = LatentModelWeights::from_gguf(content, &mut reader, device)
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
    Ok(ResidentBackbone {
        model,
        tokenizer,
        eos_ids,
        im_end_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hetero() -> HeteroTopology {
        HeteroTopology::new(
            vec![
                HeteroRoleSpec {
                    role: Role::new("Drafter"),
                    system_prompt: "Draft.".into(),
                    backbone: Qwen3Variant::Four,
                },
                HeteroRoleSpec {
                    role: Role::new("Refiner"),
                    system_prompt: "Refine.".into(),
                    backbone: Qwen3Variant::Eight,
                },
            ],
            2,
        )
    }

    #[test]
    fn distinct_backbones_and_homogeneity() {
        let t = hetero();
        assert_eq!(t.distinct_backbones().len(), 2);
        assert!(!t.is_effectively_homogeneous());

        let same = HeteroTopology::new(
            vec![
                HeteroRoleSpec {
                    role: Role::new("A"),
                    system_prompt: "a".into(),
                    backbone: Qwen3Variant::Four,
                },
                HeteroRoleSpec {
                    role: Role::new("B"),
                    system_prompt: "b".into(),
                    backbone: Qwen3Variant::Four,
                },
            ],
            1,
        );
        assert!(same.is_effectively_homogeneous());
        assert_eq!(same.distinct_backbones(), vec![Qwen3Variant::Four]);
    }

    #[test]
    fn ring_edges_cross_dimensions() {
        let t = hetero();
        // 2-role ring: Drafter(4B,2560)→Refiner(8B,4096), Refiner→Drafter.
        let edges = t.ring_edge_widths();
        assert_eq!(edges, vec![(2560, 4096), (4096, 2560)]);
    }

    #[test]
    fn schedule_and_final_hop() {
        let t = hetero();
        assert_eq!(t.schedule().len(), 4); // 2 rounds × 2 roles
        assert!(t.is_final_hop(1, 1));
        assert!(!t.is_final_hop(0, 1));
    }
}
