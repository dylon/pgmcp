//! Residency manager for the homogeneous latent loop (ADR-009 Tier-3 v1).
//!
//! The hard constraint is the 8 GB VRAM wall (`docs/memory-server/04-hardware.md`):
//! the homogeneous loop keeps exactly **one** resident backbone plus N small
//! per-role links, and must refuse to load when that footprint won't fit. This
//! module is the pre-flight: a byte-level VRAM probe (`mem_get_info` via cudarc,
//! the same handle `src/cron/gpu_fcm.rs` already uses) plus the footprint
//! arithmetic the loader checks before the expensive GGUF read.
//!
//! The arithmetic (`VramBudget::fits`, `homogeneous_footprint_bytes`) is pure and
//! unit-tested on CPU; the probe itself is GPU-gated (errors degrade the engine
//! to `None`, never panic — the same posture as `make_latent_pipeline`).
//!
//! **Feasibility envelope (corrected finding).** The plan estimated ~50 MB per
//! per-role link; the *accurate* size of an `R_in` is two `hidden×hidden` F32
//! matrices — `2·4096²·4 ≈ 134 MB` for an 8B backbone (`2·2560²·4 ≈ 52 MB` for
//! 4B). With the conservative 15% safety headroom this gate enforces, the 8B
//! backbone (~6.55 GB) therefore admits only ~1–2 resident roles on a fully-free
//! 8 GB card, while the 4B backbone (~2.6 GB) comfortably admits the full
//! multi-role loop. This matches the plan's risk-6 mitigation ("Qwen3-4B-Q4 ×2"
//! for a true multi-model loop) and is *why* the gate exists: it refuses the
//! marginal 8B configuration before an OOM mid-load rather than after.

use anyhow::{Result, anyhow};

use crate::llm::qwen3::Qwen3Variant;

/// A snapshot of device memory, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VramBudget {
    pub free_bytes: usize,
    pub total_bytes: usize,
}

impl VramBudget {
    /// Does `required_bytes` fit within the free pool, leaving `headroom_frac`
    /// (0.0–1.0) of *free* memory as slack for activations / fragmentation?
    pub fn fits(&self, required_bytes: usize, headroom_frac: f64) -> bool {
        let frac = headroom_frac.clamp(0.0, 1.0);
        let usable = (self.free_bytes as f64 * (1.0 - frac)) as usize;
        required_bytes <= usable
    }
}

/// Default activation/fragmentation slack reserved on top of the static
/// resident footprint (KV-cache, attention scratch, allocator overhead).
pub const DEFAULT_HEADROOM_FRAC: f64 = 0.15;

/// Approximate resident weight bytes of a Q4_K_M backbone (per
/// `docs/memory-server/04-hardware.md`): ~6.55 GB for 8B, ~2.6 GB for 4B.
pub fn backbone_resident_bytes(backbone: Qwen3Variant) -> usize {
    match backbone {
        Qwen3Variant::Eight => 6_550_000_000,
        Qwen3Variant::Four => 2_600_000_000,
    }
}

/// Hidden width of each backbone, known a priori so the footprint pre-flight can
/// run *before* the GGUF is read (the loaded model later reports the same value
/// via `LatentModelWeights::hidden_size`).
pub fn expected_hidden_size(backbone: Qwen3Variant) -> usize {
    match backbone {
        Qwen3Variant::Eight => 4096,
        Qwen3Variant::Four => 2560,
    }
}

/// One `R_in` link = two `hidden×hidden` F32 matrices (`W₁`, `W₂`).
pub fn link_resident_bytes(hidden_size: usize) -> usize {
    2 * hidden_size * hidden_size * std::mem::size_of::<f32>()
}

/// One `R_out` outer link = `W₁ : src×src`, `W₂ : src×tgt`, `W₃ : src×tgt`, all
/// F32 (the heterogeneous cross-dim link between two differently-sized backbones).
pub fn outer_link_resident_bytes(src_dim: usize, tgt_dim: usize) -> usize {
    (src_dim * src_dim + 2 * src_dim * tgt_dim) * std::mem::size_of::<f32>()
}

/// Estimated resident footprint of a heterogeneous loop: the sum of the distinct
/// resident backbones plus one outer link per ring edge `(src_width, tgt_width)`.
pub fn heterogeneous_footprint_bytes(
    distinct_backbones: &[Qwen3Variant],
    ring_edge_widths: &[(usize, usize)],
) -> usize {
    let backbones: usize = distinct_backbones
        .iter()
        .map(|v| backbone_resident_bytes(*v))
        .sum();
    let links: usize = ring_edge_widths
        .iter()
        .map(|&(s, t)| outer_link_resident_bytes(s, t))
        .sum();
    backbones + links
}

/// Estimated resident footprint of the homogeneous loop: one Q4 backbone plus
/// `n_links` F32 inner links. (`W₃ = I` in the homogeneous case, so there are no
/// cross-dim outer links to count.)
pub fn homogeneous_footprint_bytes(
    backbone: Qwen3Variant,
    n_links: usize,
    hidden_size: usize,
) -> usize {
    backbone_resident_bytes(backbone) + n_links * link_resident_bytes(hidden_size)
}

/// Probe live device VRAM. GPU-gated: errors (no CUDA / driver mismatch) bubble
/// up so the caller degrades the engine to `None`.
pub fn probe_cuda_vram() -> Result<VramBudget> {
    let ctx = cudarc::driver::CudaContext::new(0)
        .map_err(|e| anyhow!("CUDA context init failed: {e}"))?;
    let (free, total) = ctx
        .mem_get_info()
        .map_err(|e| anyhow!("cuMemGetInfo failed: {e}"))?;
    Ok(VramBudget {
        free_bytes: free,
        total_bytes: total,
    })
}

/// Pre-flight: would the homogeneous loop's resident footprint fit in current
/// free VRAM (with default headroom)? Probes the device and returns the verdict.
/// A probe error is surfaced (not swallowed into `Ok(false)`) so the loader can
/// log the specific cause before converting it to `None` at the factory boundary.
pub fn homogeneous_fits(
    backbone: Qwen3Variant,
    n_links: usize,
    hidden_size: usize,
) -> Result<bool> {
    let budget = probe_cuda_vram()?;
    let need = homogeneous_footprint_bytes(backbone, n_links, hidden_size);
    Ok(budget.fits(need, DEFAULT_HEADROOM_FRAC))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_respects_headroom() {
        let b = VramBudget {
            free_bytes: 8_000_000_000,
            total_bytes: 8_000_000_000,
        };
        // 15% headroom → usable 6.8 GB.
        assert!(b.fits(6_000_000_000, 0.15));
        assert!(!b.fits(7_000_000_000, 0.15));
        // Zero headroom uses the whole free pool.
        assert!(b.fits(8_000_000_000, 0.0));
        // Headroom clamps: >1.0 is treated as 1.0 (nothing usable).
        assert!(!b.fits(1, 2.0));
    }

    #[test]
    fn feasibility_envelope_matches_corrected_arithmetic() {
        // The 8 GB card (fully free) under the conservative 15% safety headroom.
        let card = VramBudget {
            free_bytes: 8_000_000_000,
            total_bytes: 8_589_934_592,
        };
        // Each 4096-wide F32 link is exactly 134 MB (the corrected size; the plan
        // estimated ~50 MB).
        assert_eq!(link_resident_bytes(4096), 2 * 4096 * 4096 * 4);
        assert_eq!(link_resident_bytes(4096), 134_217_728);

        // 8B with 1 role fits; 8B with 3 roles does NOT (6.95 GB > 6.8 GB usable)
        // — the marginal-8B finding the gate must catch.
        let need_8b_1 = homogeneous_footprint_bytes(Qwen3Variant::Eight, 1, 4096);
        let need_8b_3 = homogeneous_footprint_bytes(Qwen3Variant::Eight, 3, 4096);
        assert!(
            card.fits(need_8b_1, DEFAULT_HEADROOM_FRAC),
            "8B×1 should fit"
        );
        assert!(
            !card.fits(need_8b_3, DEFAULT_HEADROOM_FRAC),
            "8B×3 must be refused: need {need_8b_3} > usable"
        );

        // 4B is the comfortable multi-role path: 4B×3 fits with wide margin.
        let need_4b_3 = homogeneous_footprint_bytes(Qwen3Variant::Four, 3, 2560);
        assert!(
            card.fits(need_4b_3, DEFAULT_HEADROOM_FRAC),
            "4B×3 must fit comfortably"
        );

        // No backbone fits a tiny 4 GB card.
        let small = VramBudget {
            free_bytes: 4_000_000_000,
            total_bytes: 4_294_967_296,
        };
        assert!(!small.fits(
            homogeneous_footprint_bytes(Qwen3Variant::Eight, 1, 4096),
            DEFAULT_HEADROOM_FRAC
        ));
    }

    #[test]
    fn heterogeneous_4b_plus_8b_exceeds_8gb_card() {
        // A genuine cross-architecture loop (4B@2560 + 8B@4096) needs both
        // backbones resident (~9.15 GB) — over the 8 GB wall, so the gate refuses
        // it locally and it runs only on a bigger GPU / cloud (plan risk 6).
        let need = heterogeneous_footprint_bytes(
            &[Qwen3Variant::Four, Qwen3Variant::Eight],
            // 2-role ring: 4B→8B then 8B→4B.
            &[(2560, 4096), (4096, 2560)],
        );
        let card = VramBudget {
            free_bytes: 8_000_000_000,
            total_bytes: 8_589_934_592,
        };
        assert!(
            need > card.free_bytes,
            "4B+8B (~{} MB) must exceed 8 GB free",
            need >> 20
        );
        assert!(!card.fits(need, DEFAULT_HEADROOM_FRAC));
        // The same loop fits a 24 GB card comfortably.
        let big = VramBudget {
            free_bytes: 24_000_000_000,
            total_bytes: 25_769_803_776,
        };
        assert!(big.fits(need, DEFAULT_HEADROOM_FRAC));
    }

    #[test]
    fn footprint_grows_with_roles_and_width() {
        let small = homogeneous_footprint_bytes(Qwen3Variant::Four, 2, 2560);
        let big = homogeneous_footprint_bytes(Qwen3Variant::Four, 5, 2560);
        assert!(big > small);
        assert!(homogeneous_footprint_bytes(Qwen3Variant::Eight, 0, 4096) > small);
    }
}
