//! ADR-009 R4: outer-loop training for the heterogeneous latent loop.
//!
//! Trains the [`OuterLink`] `R_out` (cross-dim `W₃ : d_src→d_tgt` residual plus
//! the `W₂·σ(W₁·h)` correction) to align a *source* backbone's hidden state with
//! a *target* backbone's gold embedding space, under `1 − cos(R_out(h_src),
//! gold_tgt)`.
//!
//! **Why supervised, not end-to-end.** Through-backbone autograd is blocked — the
//! Q4-quantized backbones are not differentiable in candle — so, exactly as the
//! inner `R_in` trainer does (`src/llm/latent_train.rs`), the objective is
//! supervised on *pre-extracted* `(h_src, gold_tgt)` pairs and differentiates
//! only through `R_out` (a standalone candle `VarMap`). This is the plan's
//! "text-supervised objective first": `gold_tgt = Embed(decoded_text)` projected
//! into the target space. The frozen backbones supply `h_src` and the gold text
//! out-of-band (a separate GPU-bound extraction step), so this trainer itself
//! runs without any backbone — on CPU in CI.
//!
//! **Cross-dim is the point.** The pairs carry `src_dim ≠ tgt_dim` for genuine
//! heterogeneity (e.g. Qwen3-4B's 2560 → Qwen3-8B's 4096) — exactly the case the
//! same-dimension inner link cannot express and the outer link exists for.

#![allow(dead_code)]

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use candle_core::{DType, Device, Tensor};
use candle_nn::Optimizer;
use rand::SeedableRng;
use rand::seq::SliceRandom;
use tracing::{info, warn};

use crate::llm::latent_train::LatentTrainConfig;
use crate::llm::recursive_link::cosine_alignment_loss;
use crate::rmas::outer_link::OuterLink;

/// One `(hidden_src, gold_tgt)` training pair for the outer link.
///
/// `hidden_src` is the source backbone's post-norm last-position hidden state
/// (`src_dim`); `gold_tgt` is the L2-normalizable gold embedding in the *target*
/// backbone's space (`tgt_dim`).
#[derive(Debug, Clone)]
pub struct OuterTrainingPair {
    pub hidden_src: Vec<f32>,
    pub gold_tgt: Vec<f32>,
}

/// Result of one outer-link training run.
#[derive(Debug, Clone)]
pub struct OuterTrainReport {
    pub steps: usize,
    pub final_loss: f32,
    pub per_epoch_loss: Vec<f32>,
    pub src_dim: usize,
    pub tgt_dim: usize,
    pub link_signature: String,
}

/// Train `R_out` on the given cross-dim pairs and persist the weights. Reuses the
/// inner trainer's [`LatentTrainConfig`] (epochs/batch/lr/AdamW/seed) so the two
/// trainers share one recipe surface.
pub fn train_outer_link(
    pairs: &[OuterTrainingPair],
    src_dim: usize,
    tgt_dim: usize,
    cfg: &LatentTrainConfig,
    device: &Device,
    output_path: &Path,
    link_signature: impl Into<String>,
) -> Result<OuterTrainReport> {
    if pairs.is_empty() {
        return Err(anyhow!(
            "train_outer_link: empty training set — refuse to write degenerate weights"
        ));
    }
    for (i, p) in pairs.iter().enumerate() {
        if p.hidden_src.len() != src_dim {
            return Err(anyhow!(
                "pair {i}: hidden_src has {} entries, src_dim = {src_dim}",
                p.hidden_src.len()
            ));
        }
        if p.gold_tgt.len() != tgt_dim {
            return Err(anyhow!(
                "pair {i}: gold_tgt has {} entries, tgt_dim = {tgt_dim}",
                p.gold_tgt.len()
            ));
        }
    }

    let signature = link_signature.into();
    let dtype = DType::F32;
    let (link, varmap) = OuterLink::new(src_dim, tgt_dim, device, dtype, signature.clone())?;
    let mut optimizer = candle_nn::AdamW::new(
        varmap.all_vars(),
        candle_nn::ParamsAdamW {
            lr: cfg.learning_rate,
            beta1: cfg.adamw_beta1,
            beta2: cfg.adamw_beta2,
            eps: cfg.adamw_eps,
            weight_decay: cfg.weight_decay,
        },
    )
    .map_err(|e| anyhow!("AdamW init: {}", e))?;

    let mut indices: Vec<usize> = (0..pairs.len()).collect();
    let mut rng = rand::rngs::StdRng::seed_from_u64(cfg.seed);
    let mut per_epoch_loss = Vec::with_capacity(cfg.epochs);
    let mut final_loss: f32 = 0.0;
    let mut total_steps = 0usize;

    for epoch in 0..cfg.epochs {
        indices.shuffle(&mut rng);
        let mut epoch_loss_sum: f32 = 0.0;
        let mut epoch_step_count = 0usize;

        for batch_start in (0..indices.len()).step_by(cfg.batch_size.max(1)) {
            let batch_end = (batch_start + cfg.batch_size).min(indices.len());
            let slice = &indices[batch_start..batch_end];
            let b = slice.len();

            let hiddens: Vec<f32> = slice
                .iter()
                .flat_map(|&i| pairs[i].hidden_src.clone())
                .collect();
            let golds: Vec<f32> = slice
                .iter()
                .flat_map(|&i| pairs[i].gold_tgt.clone())
                .collect();

            let h = Tensor::from_vec(hiddens, (b, src_dim), device)?.to_dtype(dtype)?;
            let g = Tensor::from_vec(golds, (b, tgt_dim), device)?.to_dtype(dtype)?;
            let predicted = link.forward(&h)?; // (b, tgt_dim)
            let loss = cosine_alignment_loss(&predicted, &g)?;
            optimizer
                .backward_step(&loss)
                .map_err(|e| anyhow!("optimizer step: {}", e))?;

            let l_val: f32 = loss.to_dtype(DType::F32)?.to_scalar()?;
            epoch_loss_sum += l_val;
            epoch_step_count += 1;
            total_steps += 1;
            final_loss = l_val;

            if cfg.log_every > 0 && total_steps.is_multiple_of(cfg.log_every) {
                info!(
                    step = total_steps,
                    epoch,
                    loss = l_val,
                    "OuterLink trainer: step"
                );
            }
        }
        let epoch_loss = if epoch_step_count == 0 {
            0.0
        } else {
            epoch_loss_sum / epoch_step_count as f32
        };
        per_epoch_loss.push(epoch_loss);
        info!(
            epoch,
            loss = epoch_loss,
            "OuterLink trainer: epoch complete"
        );
    }

    if !final_loss.is_finite() {
        warn!(final_loss, "OuterLink trainer: non-finite final loss");
    }

    link.save(&varmap, output_path)
        .with_context(|| format!("save outer link to {}", output_path.display()))?;

    Ok(OuterTrainReport {
        steps: total_steps,
        final_loss,
        per_epoch_loss,
        src_dim,
        tgt_dim,
        link_signature: signature,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_dim_identity_target_keeps_loss_near_zero() {
        // src==tgt ⇒ W₃=I, W₂=0 ⇒ R_out is passthrough at init; gold == hidden
        // direction is the attainable cosine minimum, so loss starts/stays ~0.
        let device = Device::Cpu;
        let d = 8;
        let pairs: Vec<OuterTrainingPair> = (0..24)
            .map(|i| {
                let v: Vec<f32> = (0..d).map(|k| ((i + 1) * (k + 1)) as f32 * 0.03).collect();
                OuterTrainingPair {
                    hidden_src: v.clone(),
                    gold_tgt: v,
                }
            })
            .collect();
        let cfg = LatentTrainConfig {
            epochs: 2,
            batch_size: 4,
            learning_rate: 1e-3,
            log_every: 0,
            seed: 7,
            ..LatentTrainConfig::default()
        };
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let report = train_outer_link(&pairs, d, d, &cfg, &device, tmp.path(), "rout-v1-test")
            .expect("same-dim training must succeed");
        assert_eq!(report.src_dim, d);
        assert_eq!(report.tgt_dim, d);
        for l in &report.per_epoch_loss {
            assert!(*l < 1e-3, "identity-attainable loss must stay ~0, got {l}");
        }
    }

    #[test]
    fn cross_dim_training_runs_and_reduces_loss() {
        // src≠tgt ⇒ W₃ is a learned projection (no identity start). Train R_out
        // to map a 4-dim source onto a fixed 6-dim target direction; verify the
        // loss is finite and the final epoch improves on the first.
        let device = Device::Cpu;
        let (src, tgt) = (4usize, 6usize);
        let target_dir: Vec<f32> = vec![0.1, 0.2, 0.3, 0.4, 0.5, 0.6];
        let pairs: Vec<OuterTrainingPair> = (0..40)
            .map(|i| {
                let scale = (i + 1) as f32 * 0.05;
                OuterTrainingPair {
                    hidden_src: (0..src).map(|k| (k as f32 + 1.0) * scale).collect(),
                    gold_tgt: target_dir.iter().map(|x| x * scale).collect(),
                }
            })
            .collect();
        let cfg = LatentTrainConfig {
            epochs: 8,
            batch_size: 8,
            learning_rate: 5e-3,
            log_every: 0,
            seed: 11,
            ..LatentTrainConfig::default()
        };
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let report = train_outer_link(&pairs, src, tgt, &cfg, &device, tmp.path(), "rout-v1-xdim")
            .expect("cross-dim training must succeed");
        assert_eq!((report.src_dim, report.tgt_dim), (src, tgt));
        assert!(report.final_loss.is_finite());
        let first = report.per_epoch_loss.first().copied().expect("epochs");
        let last = report.per_epoch_loss.last().copied().expect("epochs");
        assert!(
            last <= first + 1e-4,
            "cross-dim loss should not increase: first={first}, last={last}"
        );
    }

    #[test]
    fn rejects_dim_mismatch_and_empty() {
        let device = Device::Cpu;
        let cfg = LatentTrainConfig::default();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        // gold_tgt wrong width.
        let bad = OuterTrainingPair {
            hidden_src: vec![0.0; 4],
            gold_tgt: vec![0.0; 5],
        };
        let err = train_outer_link(&[bad], 4, 8, &cfg, &device, tmp.path(), "x")
            .expect_err("dim mismatch must fail");
        assert!(format!("{err}").contains("gold_tgt"));
        let err2 = train_outer_link(&[], 4, 8, &cfg, &device, tmp.path(), "x")
            .expect_err("empty must fail");
        assert!(format!("{err2}").contains("empty"));
    }
}
