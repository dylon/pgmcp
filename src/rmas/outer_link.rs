//! The RecursiveMAS **outer link** `R_out(h) = W₃·h + W₂·σ(W₁·h)` (ADR-009
//! Track-B Tier-3; Yang et al. arXiv:2604.25917 §3.2). It bridges *heterogeneous*
//! agents: `W₃ : d_src → d_tgt` maps the source backbone's hidden state into the
//! target's embedding space (the residual branch), while `W₂·σ(W₁·h)` learns the
//! distributional correction. The inner link (`src/llm/recursive_link.rs`) is the
//! same-dimension special case (`W₃ = I`).
//!
//! At init `W₂ = 0` so the non-linear branch is inert; when `d_src == d_tgt`,
//! `W₃` is the identity, so a fresh same-dimension outer link is an exact
//! passthrough (matching the inner link's residual-identity start — the training
//! stability the paper's residual design buys). Trained via the same candle
//! `VarMap`/AdamW path as the inner link.

use anyhow::{Context, Result, anyhow};
use candle_core::{DType, Device, Tensor};
use candle_nn::{Linear, Module, VarBuilder, VarMap, linear_no_bias};
use std::path::Path;

/// Architecture signature; bump when the wiring changes.
pub const OUTER_LINK_ARCHITECTURE: &str = "rout-v1-3layer-gelu-residual";

/// `R_out` — three projections: `W₁ : d_src→d_src`, `W₂ : d_src→d_tgt`,
/// `W₃ : d_src→d_tgt` (the cross-dim residual).
pub struct OuterLink {
    w1: Linear,
    w2: Linear,
    w3: Linear,
    src_dim: usize,
    tgt_dim: usize,
    device: Device,
    dtype: DType,
    link_signature: String,
}

impl std::fmt::Debug for OuterLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OuterLink")
            .field("src_dim", &self.src_dim)
            .field("tgt_dim", &self.tgt_dim)
            .field("device", &self.device)
            .field("dtype", &self.dtype)
            .field("link_signature", &self.link_signature)
            .finish()
    }
}

impl OuterLink {
    /// Fresh outer link. `W₂ = 0` (inert non-linear branch); `W₃ = I` when
    /// `src_dim == tgt_dim` (exact passthrough start), else a learned projection.
    pub fn new(
        src_dim: usize,
        tgt_dim: usize,
        device: &Device,
        dtype: DType,
        link_signature: impl Into<String>,
    ) -> Result<(Self, VarMap)> {
        let varmap = VarMap::new();
        let vb = VarBuilder::from_varmap(&varmap, dtype, device);
        let w1 = linear_no_bias(src_dim, src_dim, vb.pp("w1"))?;
        let w2 = linear_no_bias(src_dim, tgt_dim, vb.pp("w2"))?;
        let w3 = linear_no_bias(src_dim, tgt_dim, vb.pp("w3"))?;
        {
            let data = varmap.data().lock().expect("varmap lock");
            for (name, tensor) in data.iter() {
                if name == "w2.weight" {
                    tensor.set(&Tensor::zeros_like(tensor.as_tensor())?)?;
                } else if name == "w3.weight" && src_dim == tgt_dim {
                    tensor.set(&Tensor::eye(src_dim, dtype, device)?)?;
                }
            }
        }
        Ok((
            Self {
                w1,
                w2,
                w3,
                src_dim,
                tgt_dim,
                device: device.clone(),
                dtype,
                link_signature: link_signature.into(),
            },
            varmap,
        ))
    }

    /// Apply `R_out` to a hidden-state tensor of trailing dim `src_dim`,
    /// producing trailing dim `tgt_dim`.
    pub fn forward(&self, h: &Tensor) -> Result<Tensor> {
        let last_dim = h.dim(h.rank() - 1)?;
        if last_dim != self.src_dim {
            return Err(anyhow!(
                "OuterLink::forward: trailing dim {} != src_dim {}",
                last_dim,
                self.src_dim
            ));
        }
        let nonlinear = self.w2.forward(&self.w1.forward(h)?.gelu()?)?;
        let residual = self.w3.forward(h)?;
        Ok((residual + nonlinear)?)
    }

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

    pub fn load(
        path: &Path,
        src_dim: usize,
        tgt_dim: usize,
        device: &Device,
        dtype: DType,
        link_signature: impl Into<String>,
    ) -> Result<Self> {
        let mut varmap = VarMap::new();
        let vb = VarBuilder::from_varmap(&varmap, dtype, device);
        let w1 = linear_no_bias(src_dim, src_dim, vb.pp("w1"))?;
        let w2 = linear_no_bias(src_dim, tgt_dim, vb.pp("w2"))?;
        let w3 = linear_no_bias(src_dim, tgt_dim, vb.pp("w3"))?;
        varmap
            .load(path)
            .with_context(|| format!("VarMap::load {}", path.display()))?;
        Ok(Self {
            w1,
            w2,
            w3,
            src_dim,
            tgt_dim,
            device: device.clone(),
            dtype,
            link_signature: link_signature.into(),
        })
    }

    pub fn src_dim(&self) -> usize {
        self.src_dim
    }
    pub fn tgt_dim(&self) -> usize {
        self.tgt_dim
    }
    pub fn signature(&self) -> &str {
        &self.link_signature
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_dim_outer_link_starts_as_identity() {
        let dev = Device::Cpu;
        let (link, _vm) = OuterLink::new(4, 4, &dev, DType::F32, "test").expect("build");
        let h = Tensor::from_vec(vec![1.0f32, 2.0, 3.0, 4.0], (1, 4), &dev).expect("tensor");
        let out = link.forward(&h).expect("forward");
        assert_eq!(out.dims(), &[1, 4]);
        let got = out.flatten_all().unwrap().to_vec1::<f32>().unwrap();
        // W2=0, W3=I ⇒ exact passthrough at init.
        for (a, b) in got.iter().zip([1.0f32, 2.0, 3.0, 4.0]) {
            assert!(
                (a - b).abs() < 1e-5,
                "expected identity passthrough, got {got:?}"
            );
        }
    }

    #[test]
    fn cross_dim_outer_link_maps_to_target_dim() {
        let dev = Device::Cpu;
        let (link, _vm) = OuterLink::new(4, 8, &dev, DType::F32, "test").expect("build");
        assert_eq!(link.src_dim(), 4);
        assert_eq!(link.tgt_dim(), 8);
        let h = Tensor::zeros((2, 4), DType::F32, &dev).expect("tensor");
        let out = link.forward(&h).expect("forward");
        assert_eq!(out.dims(), &[2, 8]);
    }

    #[test]
    fn forward_rejects_wrong_input_dim() {
        let dev = Device::Cpu;
        let (link, _vm) = OuterLink::new(4, 4, &dev, DType::F32, "test").expect("build");
        let h = Tensor::zeros((1, 5), DType::F32, &dev).expect("tensor");
        assert!(link.forward(&h).is_err());
    }
}
