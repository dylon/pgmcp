//! `pgmcp train-outer-link` — train an OuterLink (`R_out`) from pre-extracted
//! `(hidden_src, gold_tgt)` pairs (JSONL) and write the safetensors. Wires the
//! R4 outer-loop trainer (`src/rmas/train_outer.rs`), the cross-dim analogue of
//! `train-link`. The pairs are supplied pre-extracted (the frozen backbones'
//! through-autograd is blocked on Q4), so this runs without a backbone — on CPU.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use candle_core::Device;

use crate::llm::latent_train::LatentTrainConfig;
use crate::rmas::train_outer::{OuterTrainingPair, train_outer_link};

#[allow(clippy::too_many_arguments)]
pub async fn run(
    pairs_path: PathBuf,
    src_size: usize,
    tgt_size: usize,
    output: PathBuf,
    epochs: usize,
    learning_rate: f64,
    seed: u64,
    signature: String,
) -> Result<()> {
    crate::logging::init_cli_with_config(None);

    let text = std::fs::read_to_string(&pairs_path)
        .with_context(|| format!("read pairs JSONL {}", pairs_path.display()))?;
    let mut pairs = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value =
            serde_json::from_str(line).with_context(|| format!("parse JSONL line {}", i + 1))?;
        pairs.push(OuterTrainingPair {
            hidden_src: json_f32_vec(&v, "hidden_src")?,
            gold_tgt: json_f32_vec(&v, "gold_tgt")?,
        });
    }
    if pairs.is_empty() {
        bail!("no training pairs found in {}", pairs_path.display());
    }
    tracing::info!(
        n_pairs = pairs.len(),
        src_size,
        tgt_size,
        "train-outer-link: loaded pairs"
    );

    let device = Device::new_cuda(0).unwrap_or(Device::Cpu);
    let cfg = LatentTrainConfig {
        epochs,
        learning_rate,
        seed,
        ..LatentTrainConfig::default()
    };
    let report = train_outer_link(
        &pairs, src_size, tgt_size, &cfg, &device, &output, signature,
    )?;

    tracing::info!(
        steps = report.steps,
        final_loss = report.final_loss,
        output = %output.display(),
        "train-outer-link: training complete"
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "steps": report.steps,
            "final_loss": report.final_loss,
            "per_epoch_loss": report.per_epoch_loss,
            "src_dim": report.src_dim,
            "tgt_dim": report.tgt_dim,
            "link_signature": report.link_signature,
            "output": output.display().to_string(),
        }))?
    );
    Ok(())
}

fn json_f32_vec(v: &serde_json::Value, key: &str) -> Result<Vec<f32>> {
    let arr = v
        .get(key)
        .and_then(|x| x.as_array())
        .with_context(|| format!("missing array field '{key}'"))?;
    arr.iter()
        .map(|x| {
            x.as_f64()
                .map(|f| f as f32)
                .context("non-number element in pair array")
        })
        .collect()
}
