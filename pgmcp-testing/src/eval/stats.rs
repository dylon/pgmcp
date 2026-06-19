//! Paired statistics over per-query metric vectors — the headline
//! `semantic` vs `hybrid` vs `text` comparison.
//!
//! The unit of analysis is the **query**: for a fixed metric (e.g. nDCG@10)
//! each mode contributes one value per query, and the vectors are
//! **index-aligned by query** (the unit-key alignment the paired tests
//! require). For every unordered pair of modes we run a Wilcoxon signed-rank
//! test (paired, non-parametric — robust to the bounded, tied, bimodal IR
//! distributions), report Cliff's δ as the effect size and a seeded bootstrap
//! CI on the mean difference, and finally Benjamini-Hochberg-correct the
//! p-values across the family of pairwise comparisons. All of this delegates to
//! the validated engine in [`pgmcp::stats::inference`].
//!
//! Orientation follows `inference`'s convention: `(control, treatment)` with
//! every effect oriented **treatment − control**, so a positive Cliff's δ or
//! mean difference means the treatment mode scored higher.

use pgmcp::stats::inference::{
    BootstrapConfig, Correction, Tail, adjust_pvalues, bootstrap_diff_means, cliffs_delta,
    wilcoxon_signed_rank,
};

/// Per-mode aligned values for one metric. Every inner vector has the same
/// length and is ordered identically by query.
#[derive(Debug, Clone)]
pub struct AlignedMetric {
    pub metric: String,
    /// `(mode_tag, per_query_values)`, aligned by query across modes.
    pub by_mode: Vec<(String, Vec<f64>)>,
}

/// The outcome of one paired (control vs treatment) comparison for one metric.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PairwiseComparison {
    pub metric: String,
    pub control: String,
    pub treatment: String,
    pub n: usize,
    pub mean_control: f64,
    pub mean_treatment: f64,
    /// Point estimate of `treatment − control` (bootstrap statistic).
    pub mean_diff: f64,
    pub wilcoxon_p: f64,
    /// Benjamini-Hochberg-adjusted p across the whole pairwise family.
    pub wilcoxon_p_adj: f64,
    pub cliffs_delta: f64,
    pub effect_magnitude: String,
    pub boot_ci_low: f64,
    pub boot_ci_high: f64,
    /// `true` iff `wilcoxon_p_adj < alpha`.
    pub significant: bool,
    /// Any warnings (length mismatch, non-finite, too few samples).
    pub notes: Vec<String>,
}

/// Finite arithmetic mean (`0.0` for empty / all-non-finite).
pub fn mean(xs: &[f64]) -> f64 {
    let (s, n) = xs
        .iter()
        .filter(|v| v.is_finite())
        .fold((0.0, 0usize), |(s, n), v| (s + v, n + 1));
    if n == 0 { 0.0 } else { s / n as f64 }
}

/// Cliff's δ magnitude bins (Romano et al. 2006): |δ|<0.147 negligible,
/// <0.33 small, <0.474 medium, else large.
fn cliff_magnitude(delta: f64) -> &'static str {
    let d = delta.abs();
    if d < 0.147 {
        "negligible"
    } else if d < 0.33 {
        "small"
    } else if d < 0.474 {
        "medium"
    } else {
        "large"
    }
}

/// Run every unordered pair of modes for one metric, Benjamini-Hochberg-
/// correcting the Wilcoxon p-values across the family. The pairing is
/// `(control = by_mode[i], treatment = by_mode[j])` for `i < j`.
pub fn compare_all_pairs(m: &AlignedMetric, alpha: f64) -> Vec<PairwiseComparison> {
    let boot = BootstrapConfig {
        resamples: 10_000,
        ci_level: 0.95,
        seed: 42,
        ..BootstrapConfig::default()
    };
    let modes = &m.by_mode;
    let mut raw: Vec<PairwiseComparison> = Vec::new();

    for i in 0..modes.len() {
        for j in (i + 1)..modes.len() {
            let (ctrl_name, ctrl) = (&modes[i].0, &modes[i].1);
            let (treat_name, treat) = (&modes[j].0, &modes[j].1);
            let mut notes = Vec::new();

            let n = ctrl.len().min(treat.len());
            let (wilcoxon_p, mean_diff, ci_low, ci_high) =
                match wilcoxon_signed_rank(ctrl, treat, Tail::TwoSided) {
                    Ok(w) => {
                        let (md, lo, hi) = match bootstrap_diff_means(ctrl, treat, &boot) {
                            Ok(b) => (
                                b.statistic,
                                b.ci_low.unwrap_or(f64::NAN),
                                b.ci_high.unwrap_or(f64::NAN),
                            ),
                            Err(e) => {
                                notes.push(format!("bootstrap failed: {e}"));
                                (mean(treat) - mean(ctrl), f64::NAN, f64::NAN)
                            }
                        };
                        (w.p_value, md, lo, hi)
                    }
                    Err(e) => {
                        notes.push(format!("wilcoxon failed: {e}"));
                        (f64::NAN, mean(treat) - mean(ctrl), f64::NAN, f64::NAN)
                    }
                };

            let delta = cliffs_delta(ctrl, treat);
            raw.push(PairwiseComparison {
                metric: m.metric.clone(),
                control: ctrl_name.clone(),
                treatment: treat_name.clone(),
                n,
                mean_control: mean(ctrl),
                mean_treatment: mean(treat),
                mean_diff,
                wilcoxon_p,
                wilcoxon_p_adj: f64::NAN, // filled below
                cliffs_delta: delta,
                effect_magnitude: cliff_magnitude(delta).to_string(),
                boot_ci_low: ci_low,
                boot_ci_high: ci_high,
                significant: false,
                notes,
            });
        }
    }

    // Benjamini-Hochberg across the family of pairwise p-values. NaN p-values
    // (failed tests) are passed through unchanged by carrying them as 1.0 for
    // the adjustment, then restored to NaN.
    let ps: Vec<f64> = raw
        .iter()
        .map(|c| {
            if c.wilcoxon_p.is_finite() {
                c.wilcoxon_p
            } else {
                1.0
            }
        })
        .collect();
    let adj = adjust_pvalues(&ps, Correction::BenjaminiHochberg);
    for (c, a) in raw.iter_mut().zip(adj) {
        if c.wilcoxon_p.is_finite() {
            c.wilcoxon_p_adj = a;
            c.significant = a < alpha;
        }
    }
    raw
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vec_aligned(metric: &str, a: Vec<f64>, b: Vec<f64>, c: Vec<f64>) -> AlignedMetric {
        AlignedMetric {
            metric: metric.to_string(),
            by_mode: vec![
                ("semantic".to_string(), a),
                ("hybrid".to_string(), b),
                ("text".to_string(), c),
            ],
        }
    }

    #[test]
    fn strict_dominance_is_significant_with_large_effect() {
        // hybrid strictly > semantic on every query; text strictly worst.
        let n = 30;
        let semantic: Vec<f64> = (0..n).map(|i| 0.5 + (i as f64) * 0.001).collect();
        let hybrid: Vec<f64> = semantic.iter().map(|x| x + 0.2).collect();
        let text: Vec<f64> = semantic.iter().map(|x| x - 0.2).collect();
        let m = vec_aligned("ndcg@10", semantic, hybrid, text);
        let pairs = compare_all_pairs(&m, 0.05);
        assert_eq!(pairs.len(), 3, "3 unordered pairs of 3 modes");

        // semantic vs hybrid: treatment (hybrid) higher → positive diff/delta.
        let sh = pairs
            .iter()
            .find(|p| p.control == "semantic" && p.treatment == "hybrid")
            .unwrap();
        assert!(sh.mean_diff > 0.0, "hybrid mean higher");
        assert!(
            sh.cliffs_delta > 0.9,
            "near-total dominance → δ≈1, got {}",
            sh.cliffs_delta
        );
        assert_eq!(sh.effect_magnitude, "large");
        assert!(
            sh.significant,
            "strict dominance should be significant (p_adj={})",
            sh.wilcoxon_p_adj
        );
        assert!(sh.boot_ci_low > 0.0, "CI excludes 0");
    }

    #[test]
    fn identical_vectors_not_significant() {
        let v: Vec<f64> = (0..20).map(|i| 0.4 + (i as f64) * 0.01).collect();
        let m = vec_aligned("recall@10", v.clone(), v.clone(), v);
        let pairs = compare_all_pairs(&m, 0.05);
        for p in &pairs {
            assert!(!p.significant, "identical vectors must not be significant");
            assert!(p.cliffs_delta.abs() < 1e-9, "δ≈0 for identical");
        }
    }

    #[test]
    fn bh_adjustment_is_monotone_in_raw_p() {
        // Construct three pairs with clearly different separations; the most
        // separated pair must have the smallest adjusted p.
        let base: Vec<f64> = (0..40).map(|i| 0.3 + (i as f64) * 0.002).collect();
        let strong: Vec<f64> = base.iter().map(|x| x + 0.25).collect();
        let weak: Vec<f64> = base.iter().map(|x| x + 0.02).collect();
        let m = vec_aligned("mrr", base, strong, weak);
        let pairs = compare_all_pairs(&m, 0.05);
        // every adjusted p must be ≥ its raw p (BH only inflates).
        for p in &pairs {
            assert!(
                p.wilcoxon_p_adj + 1e-12 >= p.wilcoxon_p,
                "BH must not shrink p"
            );
        }
    }

    #[test]
    fn mean_ignores_nonfinite() {
        assert!((mean(&[1.0, 3.0, f64::NAN]) - 2.0).abs() < 1e-12);
        assert_eq!(mean(&[]), 0.0);
    }

    #[test]
    fn cliff_bins() {
        assert_eq!(cliff_magnitude(0.1), "negligible");
        assert_eq!(cliff_magnitude(0.2), "small");
        assert_eq!(cliff_magnitude(0.4), "medium");
        assert_eq!(cliff_magnitude(0.8), "large");
    }
}
