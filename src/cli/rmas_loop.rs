//! `pgmcp rmas-loop` — run a RecursiveMAS latent loop (ADR-009 R3/R4) on local
//! Qwen3 backbone(s). Builds the topology for a collaboration pattern
//! (`src/rmas/patterns.rs`), stands up the engine via the `RmasEngine` factory,
//! and runs the A₁→…→Aₙ→A₁ latent loop: intermediate roles stay in latent space,
//! only the final round's last role decodes to text.
//!
//! * No `--backbones` → **homogeneous** engine: one resident backbone (`--backbone`),
//!   per-role inner links (`W₃ = I`). The realistic single-card path (R3).
//! * `--backbones 4b,8b,…` → **heterogeneous** engine: each role pinned to the
//!   listed backbone, cross-dim outer-link hops between roles (R4). Needs every
//!   distinct backbone co-resident (research / bigger-GPU path).
//!
//! This is the engines' invocation site. Hardware-gated: without CUDA /
//! sufficient VRAM / the backbone GGUF the factory returns `None` and this
//! command reports the degradation (exit 0), pointing at the Tier-2 text path —
//! the same posture as `make_latent_pipeline`.

use std::path::PathBuf;

use anyhow::{Result, bail};

use crate::csm::role::Role;
use crate::llm::qwen3::Qwen3Variant;
use crate::rmas::hetero_loop::{HeteroRoleSpec, HeteroTopology};
use crate::rmas::patterns::{RmasPattern, rmas_topology};
use crate::rmas::{RmasEngineConfig, make_engine};

#[allow(clippy::too_many_arguments)]
pub async fn run(
    pattern: String,
    query: String,
    backbone: String,
    rounds: usize,
    n_specialists: usize,
    link_dir: PathBuf,
    max_new_tokens: usize,
    backbones: Option<String>,
) -> Result<()> {
    crate::logging::init_cli_with_config(None);

    let pattern = RmasPattern::parse(&pattern).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown pattern '{pattern}' (sequential|mixture|distillation|deliberation)"
        )
    })?;
    if query.trim().is_empty() {
        bail!("query must not be empty");
    }

    let topology = rmas_topology(pattern, rounds, n_specialists);
    let n_roles = topology.n_roles();
    let effective_rounds = topology.rounds;
    let role_names: Vec<String> = topology
        .roles
        .iter()
        .map(|r| r.role.as_str().to_string())
        .collect();
    let role_prompts: Vec<(String, String)> = topology
        .roles
        .iter()
        .map(|r| (r.role.as_str().to_string(), r.system_prompt.clone()))
        .collect();

    // Choose homogeneous vs heterogeneous from --backbones.
    let (config, kind) = match backbones {
        Some(csv) => {
            let variants = parse_backbones_csv(&csv)?;
            if variants.len() != n_roles {
                bail!(
                    "--backbones has {} entries but the '{}' pattern has {} roles",
                    variants.len(),
                    pattern.as_str(),
                    n_roles
                );
            }
            let hetero_roles: Vec<HeteroRoleSpec> = role_prompts
                .iter()
                .zip(variants)
                .map(|((name, prompt), bb)| HeteroRoleSpec {
                    role: Role::new(name.clone()),
                    system_prompt: prompt.clone(),
                    backbone: bb,
                })
                .collect();
            (
                RmasEngineConfig::Heterogeneous {
                    topology: HeteroTopology::new(hetero_roles, effective_rounds),
                    link_dir,
                },
                "heterogeneous",
            )
        }
        None => {
            let backbone_variant = parse_backbone_one(&backbone)?;
            (
                RmasEngineConfig::Homogeneous {
                    backbone: backbone_variant,
                    topology,
                    link_dir,
                },
                "homogeneous",
            )
        }
    };

    tracing::info!(
        pattern = pattern.as_str(),
        kind,
        n_roles,
        rounds = effective_rounds,
        "rmas-loop: building engine"
    );

    let engine = make_engine(config)?;
    match engine {
        Some(engine) => {
            tracing::info!(
                engine = engine.name(),
                backbone_sig = engine.backbone_signature(),
                "rmas-loop: engine ready"
            );
            let answer = engine.run_loop(&query, max_new_tokens)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "pattern": pattern.as_str(),
                    "kind": kind,
                    "engine": engine.name(),
                    "backbone_signature": engine.backbone_signature(),
                    "roles": role_names,
                    "n_roles": n_roles,
                    "rounds": effective_rounds,
                    "decoded_role": role_names.last(),
                    "answer": answer,
                }))?
            );
        }
        None => {
            tracing::error!(kind, "rmas-loop: latent engine unavailable — degraded");
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "pattern": pattern.as_str(),
                    "kind": kind,
                    "engine": "unavailable",
                    "roles": role_names,
                    "n_roles": n_roles,
                    "rounds": effective_rounds,
                    "answer": serde_json::Value::Null,
                    "reason": "no CUDA / insufficient VRAM / missing backbone — the latent loop is unavailable on this host",
                    "fallback": "use the Tier-2 text path (a2a pattern tools with recursion:{rounds}) for a black-box-compatible run",
                }))?
            );
        }
    }
    Ok(())
}

fn parse_backbone_one(s: &str) -> Result<Qwen3Variant> {
    match s.trim().to_ascii_lowercase().as_str() {
        "8b" | "8" | "eight" => Ok(Qwen3Variant::Eight),
        "4b" | "4" | "four" => Ok(Qwen3Variant::Four),
        other => bail!("unknown backbone '{other}' (8b|4b)"),
    }
}

fn parse_backbones_csv(csv: &str) -> Result<Vec<Qwen3Variant>> {
    csv.split(',')
        .filter(|s| !s.trim().is_empty())
        .map(parse_backbone_one)
        .collect()
}
