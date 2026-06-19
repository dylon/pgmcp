//! RecursiveMAS Tier-3 latent engine (ADR-009 Track B; Yang et al.
//! arXiv:2604.25917). White-box only: the latent loop transfers hidden states
//! between local open-weights backbones via RecursiveLink, building on
//! `src/llm/` (candle local inference). Black-box agents (Claude/Codex) cannot
//! join the latent loop — they sit at Text edges (enforced by `csm::media`).
//!
//! As-built (R3 + R4 shipped):
//! - [`loop_runner::HomogeneousQwen3Engine`] (R3) — one resident backbone,
//!   per-role inner links (`W₃ = I`); the realistic single-card path.
//! - [`hetero_loop::HeterogeneousQwen3Engine`] (R4) — multiple resident
//!   backbones, cross-dim outer-link hops; needs every distinct backbone
//!   co-resident (research / bigger-GPU path — plan risk 6).
//! - [`outer_link::OuterLink`] (`R_out`) + [`link_registry`] + [`train_outer`]
//!   (the cross-dim trainer) + [`patterns`] (the 4 patterns → topology) +
//!   [`residency`] (the VRAM gate).
//!
//! Both engines are reached via `pgmcp rmas-loop` and the [`make_engine`]
//! factory, which returns `Ok(None)` (degrade to the Tier-2 text path) when the
//! hardware/weights are unavailable — the same posture as `make_latent_pipeline`.
//!
//! `#![allow(dead_code)]` remains because the GPU-resident inference paths
//! (`load`/`run_loop` interiors) and parts of the config/registry API surface are
//! exercised only on a GPU host, not in CPU CI; the pure data + arithmetic +
//! trainers are unit-tested on CPU tensors.

#![allow(dead_code)]

pub mod hetero_loop;
pub mod link_registry;
pub mod loop_runner;
pub mod outer_link;
pub mod patterns;
pub mod residency;
pub mod topology;
pub mod train_outer;

use std::path::PathBuf;

use anyhow::Result;

use crate::llm::qwen3::Qwen3Variant;
use crate::rmas::hetero_loop::{HeteroTopology, HeterogeneousQwen3Engine};
use crate::rmas::loop_runner::HomogeneousQwen3Engine;
use crate::rmas::topology::RmasTopology;

/// The latent-engine seam — mirrors `fcm::FcmBackend` / `llm::LatentPipeline`:
/// swappable impls behind a trait, a closed construction-time choice enum, never
/// feature-gated (the project has no `[features]`).
pub trait RmasEngine: Send + Sync {
    /// Engine kind (telemetry / tool reporting).
    fn name(&self) -> &'static str;
    /// Backbone weight signature (the latent space all roles share).
    fn backbone_signature(&self) -> &'static str;
    /// Run the latent loop on `query` for the topology's rounds; only the final
    /// round's last role decodes to text (the rest stay latent). Returns the
    /// decoded answer.
    fn run_loop(&self, query: &str, max_new_tokens: usize) -> Result<String>;
}

/// Closed construction-time choice of latent engine kind (cf. `BackendChoice`),
/// as selected from config strings. The full per-kind data lives in
/// [`RmasEngineConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RmasBackendChoice {
    /// One resident Qwen3 backbone, per-role links + prompt swaps (`W₃ = I`).
    HomogeneousQwen3,
    /// Multiple resident backbones, cross-dim outer-link hops (research / cloud).
    HeterogeneousLocal,
    /// No latent engine — callers degrade to the Tier-2 text path.
    Disabled,
}

/// Parse a config string into a backend choice (default `Disabled`).
pub fn parse_rmas_choice(s: &str) -> RmasBackendChoice {
    match s.trim().to_ascii_lowercase().as_str() {
        "homogeneous-qwen3" | "homogeneous" | "qwen3" => RmasBackendChoice::HomogeneousQwen3,
        "heterogeneous-local" | "heterogeneous" | "hetero" => RmasBackendChoice::HeterogeneousLocal,
        _ => RmasBackendChoice::Disabled,
    }
}

/// Everything the factory needs to stand up a latent engine. The homogeneous and
/// heterogeneous cases carry genuinely different data (one backbone + an
/// [`RmasTopology`] vs. per-role backbones in a [`HeteroTopology`]), so this is a
/// closed enum rather than a struct with optional fields.
pub enum RmasEngineConfig {
    /// One resident backbone; roles differ only by prompt + inner link.
    Homogeneous {
        backbone: Qwen3Variant,
        topology: RmasTopology,
        /// Directory of per-role link safetensors (`rin__<role>.safetensors`); a
        /// role with no file gets a residual-identity passthrough link.
        link_dir: PathBuf,
    },
    /// Roles pinned to (possibly) different backbones; cross-dim outer-link hops.
    Heterogeneous {
        topology: HeteroTopology,
        /// Directory of per-edge outer-link safetensors
        /// (`rout__<src>__<tgt>.safetensors`); an edge with no file gets a fresh
        /// outer link (identity residual when widths match).
        link_dir: PathBuf,
    },
    /// No engine.
    Disabled,
}

/// Build the latent engine, or `Ok(None)` when it is disabled or the hardware /
/// weights are unavailable — the same degradation posture as
/// `llm::make_latent_pipeline` and `fcm::make_backend`, so a caller can fall
/// back to the Tier-2 text path without treating absence as an error.
pub fn make_engine(cfg: RmasEngineConfig) -> Result<Option<Box<dyn RmasEngine>>> {
    match cfg {
        RmasEngineConfig::Disabled => Ok(None),
        RmasEngineConfig::Homogeneous {
            backbone,
            topology,
            link_dir,
        } => match HomogeneousQwen3Engine::load(backbone, topology, &link_dir) {
            Ok(engine) => Ok(Some(Box::new(engine) as Box<dyn RmasEngine>)),
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "rmas: HomogeneousQwen3 engine unavailable (no GPU / missing backbone / load error) — degrading to text path"
                );
                Ok(None)
            }
        },
        RmasEngineConfig::Heterogeneous { topology, link_dir } => {
            match HeterogeneousQwen3Engine::load(topology, &link_dir) {
                Ok(engine) => Ok(Some(Box::new(engine) as Box<dyn RmasEngine>)),
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "rmas: HeterogeneousQwen3 engine unavailable (no GPU / insufficient VRAM / missing backbone) — degrading to text path"
                    );
                    Ok(None)
                }
            }
        }
    }
}

#[cfg(test)]
mod factory_tests {
    use super::*;

    fn sample_topology() -> RmasTopology {
        RmasTopology::homogeneous(
            vec![
                ("Planner".into(), "Plan the solution.".into()),
                ("Solver".into(), "Solve it.".into()),
            ],
            2,
        )
    }

    fn sample_hetero_topology() -> HeteroTopology {
        use crate::csm::role::Role;
        use crate::rmas::hetero_loop::HeteroRoleSpec;
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
    fn disabled_choice_yields_no_engine() {
        let engine = make_engine(RmasEngineConfig::Disabled).expect("Disabled never errors");
        assert!(engine.is_none());
    }

    #[test]
    fn homogeneous_degrades_to_none_without_gpu() {
        // No CUDA / no backbone in CI: `load` errors, `make_engine` returns
        // `Ok(None)` (it must never propagate the hardware-absence error).
        let engine = make_engine(RmasEngineConfig::Homogeneous {
            backbone: Qwen3Variant::Eight,
            topology: sample_topology(),
            link_dir: PathBuf::from("/nonexistent"),
        })
        .expect("hardware absence must degrade, not error");
        // On a GPU host with the backbone present this could be Some; the
        // invariant under test is that absence is not an error.
        let _ = engine;
    }

    #[test]
    fn heterogeneous_degrades_to_none_without_gpu() {
        let engine = make_engine(RmasEngineConfig::Heterogeneous {
            topology: sample_hetero_topology(),
            link_dir: PathBuf::from("/nonexistent"),
        })
        .expect("hardware absence must degrade, not error");
        let _ = engine;
    }

    #[test]
    fn choice_parsing_defaults_to_disabled() {
        assert_eq!(
            parse_rmas_choice("homogeneous-qwen3"),
            RmasBackendChoice::HomogeneousQwen3
        );
        assert_eq!(
            parse_rmas_choice("qwen3"),
            RmasBackendChoice::HomogeneousQwen3
        );
        assert_eq!(
            parse_rmas_choice("heterogeneous"),
            RmasBackendChoice::HeterogeneousLocal
        );
        assert_eq!(parse_rmas_choice("off"), RmasBackendChoice::Disabled);
        assert_eq!(parse_rmas_choice(""), RmasBackendChoice::Disabled);
    }
}
