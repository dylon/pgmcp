//! Regression test for Phase 11 (bf16 auto-adjust path).
//!
//! `GpuPrecision::auto_adjust_for_un_normalized` upgrades the fp16
//! selector to bf16 when input magnitudes exceed fp16's saturation
//! threshold (~65504 in theory; we use 1000 as a guard well below the
//! limit since fp16 round-off is already lossy long before saturation).
//! This test guards the policy by exercising every precision and
//! magnitude combination.

use pgmcp::fcm::GpuPrecision;

#[test]
fn fp16_stays_fp16_when_values_are_normalized() {
    // L2-normalized embeddings have |v| ≤ 1.0 component-wise; the
    // auto-adjust must not kick in here.
    for max_abs in [0.0_f32, 0.1, 0.5, 1.0, 10.0, 100.0, 999.9] {
        let adjusted = GpuPrecision::Fp16.auto_adjust_for_un_normalized(max_abs);
        assert_eq!(
            adjusted,
            GpuPrecision::Fp16,
            "fp16 must stay fp16 at max_abs={max_abs}"
        );
    }
}

#[test]
fn fp16_upgrades_to_bf16_above_1000() {
    // Strict inequality `> 1000.0`: the boundary itself stays fp16,
    // anything beyond it upgrades. Matches the impl at src/fcm/mod.rs:89.
    let at_boundary = GpuPrecision::Fp16.auto_adjust_for_un_normalized(1000.0);
    assert_eq!(at_boundary, GpuPrecision::Fp16);

    for max_abs in [1000.1_f32, 5000.0, 30_000.0, 65_504.0, 100_000.0] {
        let adjusted = GpuPrecision::Fp16.auto_adjust_for_un_normalized(max_abs);
        assert_eq!(
            adjusted,
            GpuPrecision::Bf16,
            "fp16 must upgrade to bf16 at max_abs={max_abs}"
        );
    }
}

#[test]
fn fp32_and_bf16_are_never_adjusted() {
    // Auto-adjust only applies to fp16; other precisions pass through
    // unchanged regardless of magnitude.
    for max_abs in [0.0_f32, 1.0, 1000.0, 100_000.0] {
        assert_eq!(
            GpuPrecision::Fp32.auto_adjust_for_un_normalized(max_abs),
            GpuPrecision::Fp32,
            "fp32 must stay fp32"
        );
        assert_eq!(
            GpuPrecision::Bf16.auto_adjust_for_un_normalized(max_abs),
            GpuPrecision::Bf16,
            "bf16 must stay bf16"
        );
    }
}

#[test]
fn parse_recognizes_all_documented_precision_strings() {
    assert_eq!(GpuPrecision::parse("fp16"), GpuPrecision::Fp16);
    assert_eq!(GpuPrecision::parse("FP16"), GpuPrecision::Fp16);
    assert_eq!(GpuPrecision::parse("f16"), GpuPrecision::Fp16);
    assert_eq!(GpuPrecision::parse("bf16"), GpuPrecision::Bf16);
    assert_eq!(GpuPrecision::parse("BF16"), GpuPrecision::Bf16);
    assert_eq!(GpuPrecision::parse("bfloat16"), GpuPrecision::Bf16);
    assert_eq!(GpuPrecision::parse("fp32"), GpuPrecision::Fp32);
    assert_eq!(GpuPrecision::parse(""), GpuPrecision::Fp32);
    assert_eq!(GpuPrecision::parse("garbage"), GpuPrecision::Fp32);
}
