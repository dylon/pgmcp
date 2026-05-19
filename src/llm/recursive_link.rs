//! Memory-server Phase 11: RecursiveLink (`R_in`) candle module.
//!
//! Implements the 2-layer residual projection from Yang et al. *Recursive
//! Multi-Agent Systems* (arXiv:2604.25917, §3.2):
//!
//! ```text
//!     R_in(h) = h + W_2 · σ(W_1 · h)
//! ```
//!
//! where `σ` is GELU and `W_1, W_2 ∈ ℝ^{d_h × d_h}` are trainable.
//! The map is a residual identity at initialization (`W_2 = 0`), so a
//! freshly-spawned RecursiveLink leaves the underlying LLM's hidden
//! state untouched — training only adds task-specific corrections.
//!
//! pgmcp keeps `R_in` per (backbone, link_signature) on disk as a
//! safetensors file: see `docs/memory-server/02-phases.md` Phase 11.2.
//! At inference time the dispatcher loads it into memory once and
//! re-uses it for every pipeline stage hand-off.

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use candle_core::{DType, Device, Tensor};
use candle_nn::{Linear, Module, VarBuilder, VarMap, linear_no_bias};

/// Signature stamped into the safetensors metadata. Bump when the
/// architecture (layer count, activation, residual wiring) changes.
pub const RECURSIVE_LINK_ARCHITECTURE: &str = "rlv1-2layer-gelu-residual";

/// `R_in` itself: two `Linear`-style projections with a GELU between.
///
/// The structure is intentionally close to a transformer FFN block so
/// candle's optimizer machinery (`VarMap`) treats it uniformly during
/// training.
pub struct RecursiveLink {
    w1: Linear,
    w2: Linear,
    hidden_size: usize,
    device: Device,
    dtype: DType,
    link_signature: String,
}

impl std::fmt::Debug for RecursiveLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecursiveLink")
            .field("hidden_size", &self.hidden_size)
            .field("device", &self.device)
            .field("dtype", &self.dtype)
            .field("link_signature", &self.link_signature)
            .finish()
    }
}

impl RecursiveLink {
    /// Build a fresh RecursiveLink with the residual-identity init
    /// (`W_2 = 0`). Used by the trainer; not the dispatcher.
    pub fn new_residual_identity(
        hidden_size: usize,
        device: &Device,
        dtype: DType,
        link_signature: impl Into<String>,
    ) -> Result<(Self, VarMap)> {
        let varmap = VarMap::new();
        let vb = VarBuilder::from_varmap(&varmap, dtype, device);
        let w1 = linear_no_bias(hidden_size, hidden_size, vb.pp("w1"))?;
        let w2 = linear_no_bias(hidden_size, hidden_size, vb.pp("w2"))?;
        // Re-init W_2 to zero so R_in starts as identity.
        for (name, tensor) in varmap.data().lock().unwrap().iter() {
            if name == "w2.weight" {
                let zero = Tensor::zeros_like(tensor.as_tensor())?;
                tensor.set(&zero)?;
            }
        }
        Ok((
            Self {
                w1,
                w2,
                hidden_size,
                device: device.clone(),
                dtype,
                link_signature: link_signature.into(),
            },
            varmap,
        ))
    }

    /// Apply `R_in` to a hidden-state tensor of shape
    /// `(.., hidden_size)`. The residual is added so a zero-initialized
    /// `W_2` makes this an identity map.
    pub fn forward(&self, h: &Tensor) -> Result<Tensor> {
        let last_dim = h.dim(h.rank() - 1)?;
        if last_dim != self.hidden_size {
            return Err(anyhow!(
                "RecursiveLink::forward: trailing dim {} != hidden_size {}",
                last_dim,
                self.hidden_size
            ));
        }
        let projected = self.w1.forward(h)?;
        let activated = projected.gelu()?;
        let delta = self.w2.forward(&activated)?;
        let out = (h + delta)?;
        Ok(out)
    }

    /// Persist the link to a safetensors file. Round-trip preserves
    /// hidden_size + signature; verify with `signature()` before use.
    pub fn save(&self, varmap: &VarMap, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create_dir_all {}", parent.display()))?;
        }
        varmap
            .save(path)
            .with_context(|| format!("VarMap::save {}", path.display()))?;
        Ok(())
    }

    /// Load a previously-saved link. Sanity-checks dimensions against
    /// the hidden_size of the backbone we plan to pair it with.
    pub fn load(
        path: &Path,
        hidden_size: usize,
        device: &Device,
        dtype: DType,
        link_signature: impl Into<String>,
    ) -> Result<Self> {
        // VarMap::load only updates entries that already exist by name, so
        // we have to construct the Linear modules first (which inserts
        // "w1.weight" / "w2.weight" into the varmap) and only then load
        // the saved tensor values into those slots.
        let mut varmap = VarMap::new();
        let vb = VarBuilder::from_varmap(&varmap, dtype, device);
        let w1 = linear_no_bias(hidden_size, hidden_size, vb.pp("w1"))?;
        let w2 = linear_no_bias(hidden_size, hidden_size, vb.pp("w2"))?;
        varmap
            .load(path)
            .with_context(|| format!("VarMap::load {}", path.display()))?;
        Ok(Self {
            w1,
            w2,
            hidden_size,
            device: device.clone(),
            dtype,
            link_signature: link_signature.into(),
        })
    }

    pub fn signature(&self) -> &str {
        &self.link_signature
    }

    pub fn hidden_size(&self) -> usize {
        self.hidden_size
    }

    pub fn device(&self) -> &Device {
        &self.device
    }

    pub fn dtype(&self) -> DType {
        self.dtype
    }
}

/// Training-step loss: `1 − cos(R_in(h), gold_embed)`. Used by the
/// one-shot RecursiveLink trainer (Phase 11.2). Both inputs share the
/// same rank-2 shape `(batch, hidden_size)`.
pub fn cosine_alignment_loss(predicted: &Tensor, gold: &Tensor) -> Result<Tensor> {
    let p = l2_normalize_last_dim(predicted)?;
    let g = l2_normalize_last_dim(gold)?;
    let dot = (&p * &g)?.sum_keepdim(p.rank() - 1)?;
    let ones = Tensor::ones_like(&dot)?;
    let loss = (ones - dot)?.mean_all()?;
    Ok(loss)
}

fn l2_normalize_last_dim(t: &Tensor) -> Result<Tensor> {
    let squared = t.sqr()?;
    let sum = squared.sum_keepdim(t.rank() - 1)?;
    let eps = Tensor::full(1e-12_f32, sum.shape(), t.device())?.to_dtype(t.dtype())?;
    let norm = (sum + eps)?.sqrt()?;
    Ok(t.broadcast_div(&norm)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn residual_identity_init_returns_input_unchanged() {
        let device = Device::Cpu;
        let dtype = DType::F32;
        let (link, _vm) = RecursiveLink::new_residual_identity(8, &device, dtype, "test").unwrap();
        let h = Tensor::from_vec((0..32).map(|i| i as f32).collect(), (4, 8), &device).unwrap();
        let out = link.forward(&h).unwrap();
        let diff = (h - out).unwrap().abs().unwrap().sum_all().unwrap();
        let v: f32 = diff.to_scalar().unwrap();
        assert!(v < 1e-5, "expected identity init to be lossless, got {v}");
    }

    #[test]
    fn forward_rejects_dim_mismatch() {
        let device = Device::Cpu;
        let (link, _vm) =
            RecursiveLink::new_residual_identity(8, &device, DType::F32, "test").unwrap();
        let bad = Tensor::zeros((4, 9), DType::F32, &device).unwrap();
        let err = link.forward(&bad).expect_err("dim mismatch must fail");
        let s = format!("{err}");
        assert!(s.contains("hidden_size"), "wrong err: {s}");
    }

    #[test]
    fn cosine_alignment_loss_is_zero_for_identical_inputs() {
        let device = Device::Cpu;
        let dtype = DType::F32;
        let t = Tensor::from_vec((0..32).map(|i| (i + 1) as f32).collect(), (4, 8), &device)
            .unwrap()
            .to_dtype(dtype)
            .unwrap();
        let loss = cosine_alignment_loss(&t, &t).unwrap();
        let v: f32 = loss.to_scalar().unwrap();
        assert!(
            v.abs() < 1e-5,
            "identical inputs must have ~0 loss, got {v}"
        );
    }

    #[test]
    fn cosine_alignment_loss_is_two_for_opposite_inputs() {
        let device = Device::Cpu;
        let dtype = DType::F32;
        let a = Tensor::from_vec(vec![1.0_f32, 0.0, 0.0, 0.0], (1, 4), &device)
            .unwrap()
            .to_dtype(dtype)
            .unwrap();
        let b = Tensor::from_vec(vec![-1.0_f32, 0.0, 0.0, 0.0], (1, 4), &device)
            .unwrap()
            .to_dtype(dtype)
            .unwrap();
        let loss = cosine_alignment_loss(&a, &b).unwrap();
        let v: f32 = loss.to_scalar().unwrap();
        assert!(
            (v - 2.0).abs() < 1e-5,
            "opposite vectors must have loss=2, got {v}"
        );
    }

    #[test]
    fn save_and_load_round_trip() {
        let device = Device::Cpu;
        let dtype = DType::F32;
        let (link, vm) = RecursiveLink::new_residual_identity(4, &device, dtype, "rlv1").unwrap();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        link.save(&vm, tmp.path()).unwrap();
        let loaded =
            RecursiveLink::load(tmp.path(), 4, &device, dtype, "rlv1").expect("load round-trip");
        let h = Tensor::from_vec(vec![1.0_f32, 2.0, 3.0, 4.0], (1, 4), &device).unwrap();
        let a = link.forward(&h).unwrap();
        let b = loaded.forward(&h).unwrap();
        let diff: f32 = (&a - &b)
            .unwrap()
            .abs()
            .unwrap()
            .sum_all()
            .unwrap()
            .to_scalar()
            .unwrap();
        assert!(
            diff < 1e-5,
            "round-tripped link must produce identical output, diff = {diff}"
        );
    }
}
