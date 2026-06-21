//! Inferential statistics for the scientific-experiment subsystem.
//!
//! This module is self-contained: it implements the hypothesis tests,
//! effect sizes, bootstrap confidence intervals, equivalence testing, and
//! multiple-comparison corrections that the experiment subsystem
//! (`src/experiment/`) uses to render empirical accept/reject verdicts. The
//! only external dependency is `statrs`, used purely for the numerically
//! validated distribution CDFs / quantiles (Student-t, normal, χ²) — the
//! regularized-incomplete-beta / erf tails are the one genuinely
//! error-prone piece, so we lean on a vetted implementation there and own
//! everything else (means, variances, ranks, resampling).
//!
//! ## Convention
//!
//! Two-sample functions take `(control, treatment)` and orient every effect
//! and difference as **treatment − control**: a positive difference / effect
//! means the treatment increased the metric. The caller (acceptance layer)
//! maps "lower is better" / predicted direction onto the [`Tail`].
//!
//! ## Why both parametric and non-parametric
//!
//! Systems/latency/throughput distributions are routinely non-normal —
//! right-skewed (a hard floor on the fast path plus a long slow tail),
//! multimodal (warm-up vs steady state, cold vs warm cache, scheduler
//! migration across CCDs/NUMA), heavy-tailed (tail latency is the point of a
//! p99). Welch's t assumes approximate normality of the sampling
//! distribution of the mean; under heavy tails at modest n that fails and
//! the p-value is miscalibrated. [`mann_whitney_u`] (rank-based) and
//! [`bootstrap_diff_means`] (resampling) stay valid there, which is why the
//! benchmarking literature (Georges 2007; Kalibera-Jones 2013) prefers CIs +
//! distribution-free comparison over bare mean ± stddev.
//!
//! ## References
//!
//! - Welch, B. L. (1947). *Biometrika* 34. (unequal-variance t)
//! - Satterthwaite, F. E. (1946). *Biometrics Bulletin* 2(6). (effective df)
//! - Mann, H. B. & Whitney, D. R. (1947). *Ann. Math. Statist.* 18.
//! - Wilcoxon, F. (1945). *Biometrics Bulletin* 1(6). (signed-rank)
//! - Efron, B. (1987). *JASA* 82; DiCiccio & Efron (1996). *Statist. Sci.* 11. (BCa)
//! - Cohen, J. (1988). *Statistical Power Analysis*. (d; sample-size formula)
//! - Cliff, N. (1993). *Psychological Bulletin* 114. (δ)
//! - Stephens, M. A. (1974). *JASA* 69. (Anderson-Darling normality)
//! - D'Agostino, R. & Pearson, E. S. (1973). *Biometrika* 60. (K² omnibus)
//! - Schuirmann, D. J. (1987). *J. Pharmacokinet. Biopharm.* 15; Lakens (2017). *SPPS* 8. (TOST)
//! - Benjamini, Y. & Hochberg, Y. (1995). *JRSS-B* 57. (FDR)
//! - Welford, B. P. (1962). *Technometrics* 4(3). (online variance)
//! - Georges et al. (2007), OOPSLA; Kalibera & Jones (2013), ISMM. (rigorous benchmarking)

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};
use statrs::distribution::{ChiSquared, ContinuousCDF, Normal, StudentsT};

/// The statistical procedure that produced a [`TestResult`]. Non-NHST kinds
/// (the threshold/relative/observational leaves of an acceptance criterion)
/// also flow through `TestResult` for a uniform evidence record; for those
/// `p_value` is `NaN` (serializes as JSON `null`) because no null hypothesis
/// is being tested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestKind {
    WelchT,
    MannWhitneyU,
    WilcoxonSignedRank,
    BootstrapDiffMeans,
    BootstrapDiffMedians,
    Tost,
    AbsoluteThreshold,
    RelativeImprovement,
    EffectThreshold,
    Observational,
}

/// Alternative-hypothesis sidedness, expressed in terms of the
/// treatment − control difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tail {
    /// H₁: treatment ≠ control.
    TwoSided,
    /// H₁: treatment < control (treatment decreases the metric).
    Less,
    /// H₁: treatment > control (treatment increases the metric).
    Greater,
}

/// Which effect-size estimator a magnitude refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectKind {
    CohensD,
    HedgesG,
    CliffsDelta,
    RankBiserial,
    RelativeChange,
}

/// Uniform result envelope from every test. Test-specific fields are
/// optional; `notes` carries warnings (non-normality, low n, ties, …).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestResult {
    pub kind: TestKind,
    pub tail: Tail,
    /// The test statistic (t, z, U, W⁺, or the observed estimand).
    pub statistic: f64,
    /// Welch–Satterthwaite degrees of freedom (t-tests only).
    pub df: Option<f64>,
    /// Two- or one-sided p-value. `NaN` for non-NHST kinds.
    pub p_value: f64,
    pub effect_size: Option<f64>,
    pub effect_kind: Option<EffectKind>,
    /// CI on the estimand (difference of means/medians, or the arm summary).
    pub ci_low: Option<f64>,
    pub ci_high: Option<f64>,
    pub ci_level: f64,
    pub n_control: usize,
    pub n_treatment: usize,
    pub notes: Vec<String>,
}

/// Descriptive summary of one sample, computed in a single Welford pass plus
/// one sort for the order statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SampleSummary {
    pub n: usize,
    pub mean: f64,
    pub variance: f64,
    pub std_dev: f64,
    pub min: f64,
    pub max: f64,
    pub median: f64,
    pub p25: f64,
    pub p75: f64,
}

/// Errors from the inference layer. Distinct from a generic "bad input"
/// string so the acceptance layer can react (e.g. report `inconclusive`
/// rather than failing the whole decision).
#[derive(Debug, thiserror::Error)]
pub enum StatsError {
    #[error("sample too small: need >= {min}, got {got}")]
    TooFewSamples { min: usize, got: usize },
    #[error("paired samples differ in length: control={n_control}, treatment={n_treatment}")]
    LengthMismatch {
        n_control: usize,
        n_treatment: usize,
    },
    #[error("zero variance in both samples; test undefined")]
    DegenerateVariance,
    #[error("non-finite value at index {0}")]
    NonFinite(usize),
    #[error("invalid parameter: {0}")]
    InvalidParam(String),
}

// ============================================================================
// Numeric primitives
// ============================================================================

fn standard_normal() -> Normal {
    Normal::new(0.0, 1.0).expect("standard normal is well-defined")
}

fn validate_finite(samples: &[f64]) -> Result<(), StatsError> {
    for (i, &x) in samples.iter().enumerate() {
        if !x.is_finite() {
            return Err(StatsError::NonFinite(i));
        }
    }
    Ok(())
}

/// Welford's online algorithm (1962). Returns `(n, mean, m2)` where the
/// unbiased sample variance is `m2 / (n - 1)`. Single pass, no per-element
/// allocation, and no catastrophic cancellation from `E[x²] − E[x]²`.
pub fn welford(samples: &[f64]) -> (usize, f64, f64) {
    let mut n = 0_usize;
    let mut mean = 0.0_f64;
    let mut m2 = 0.0_f64;
    for &x in samples {
        n += 1;
        let delta = x - mean;
        mean += delta / n as f64;
        let delta2 = x - mean;
        m2 += delta * delta2;
    }
    (n, mean, m2)
}

#[inline]
fn mean(samples: &[f64]) -> f64 {
    welford(samples).1
}

/// Unbiased sample variance (n − 1 denominator). 0.0 for n < 2.
fn sample_variance(samples: &[f64]) -> f64 {
    let (n, _, m2) = welford(samples);
    if n < 2 { 0.0 } else { m2 / (n as f64 - 1.0) }
}

/// Linear-interpolated quantile (numpy/`'linear'`, Hyndman-Fan type 7) over a
/// pre-sorted slice. `q` in `[0, 1]`.
fn percentile_sorted(sorted: &[f64], q: f64) -> f64 {
    let n = sorted.len();
    if n == 0 {
        return f64::NAN;
    }
    if n == 1 {
        return sorted[0];
    }
    let pos = q * (n as f64 - 1.0);
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    if lo == hi {
        sorted[lo]
    } else {
        let frac = pos - lo as f64;
        sorted[lo] * (1.0 - frac) + sorted[hi] * frac
    }
}

fn sorted_copy(samples: &[f64]) -> Vec<f64> {
    let mut v = samples.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    v
}

/// Median over an unsorted slice (sorts a copy).
pub fn median(samples: &[f64]) -> f64 {
    percentile_sorted(&sorted_copy(samples), 0.5)
}

/// Linear-interpolated quantile (`q` ∈ `[0, 1]`) over an unsorted slice
/// (sorts a copy). Used by absolute-threshold acceptance criteria for
/// p95/p99-style SLOs.
pub fn percentile(samples: &[f64], q: f64) -> f64 {
    percentile_sorted(&sorted_copy(samples), q)
}

/// Full descriptive summary.
pub fn summarize(samples: &[f64]) -> SampleSummary {
    let (n, mean_v, m2) = welford(samples);
    let variance = if n < 2 { 0.0 } else { m2 / (n as f64 - 1.0) };
    let sorted = sorted_copy(samples);
    SampleSummary {
        n,
        mean: mean_v,
        variance,
        std_dev: variance.sqrt(),
        min: sorted.first().copied().unwrap_or(f64::NAN),
        max: sorted.last().copied().unwrap_or(f64::NAN),
        median: percentile_sorted(&sorted, 0.5),
        p25: percentile_sorted(&sorted, 0.25),
        p75: percentile_sorted(&sorted, 0.75),
    }
}

/// Average-rank assignment (1-based; tied values share the mean of their
/// ranks), returned aligned to the input order. Also returns the
/// `Σ(tⱼ³ − tⱼ)` tie term used by the rank-test variance corrections.
fn average_ranks(values: &[f64]) -> (Vec<f64>, f64) {
    let n = values.len();
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by(|&a, &b| values[a].partial_cmp(&values[b]).unwrap_or(Ordering::Equal));
    let mut ranks = vec![0.0_f64; n];
    let mut tie_term = 0.0_f64;
    let mut i = 0;
    while i < n {
        let mut j = i;
        while j + 1 < n && values[idx[j + 1]] == values[idx[i]] {
            j += 1;
        }
        let group = (j - i + 1) as f64;
        let avg_rank = ((i + 1) + (j + 1)) as f64 / 2.0;
        for &k in &idx[i..=j] {
            ranks[k] = avg_rank;
        }
        tie_term += group * group * group - group;
        i = j + 1;
    }
    (ranks, tie_term)
}

/// Two-sided p from a standard-normal z (|z| ≥ 0), clamped to `[0, 1]`.
fn two_sided_normal_p(z: f64) -> f64 {
    let n = standard_normal();
    (2.0 * (1.0 - n.cdf(z.abs()))).clamp(0.0, 1.0)
}

fn one_sided_normal_p(z: f64, tail: Tail) -> f64 {
    let n = standard_normal();
    match tail {
        Tail::Greater => (1.0 - n.cdf(z)).clamp(0.0, 1.0),
        Tail::Less => n.cdf(z).clamp(0.0, 1.0),
        Tail::TwoSided => two_sided_normal_p(z),
    }
}

// ============================================================================
// Effect sizes
// ============================================================================

/// Cohen's d with the pooled standard deviation (Cohen 1988). Sign follows
/// treatment − control.
pub fn cohens_d(control: &[f64], treatment: &[f64]) -> f64 {
    let n1 = control.len() as f64;
    let n2 = treatment.len() as f64;
    if n1 < 2.0 || n2 < 2.0 {
        return f64::NAN;
    }
    let v1 = sample_variance(control);
    let v2 = sample_variance(treatment);
    let pooled = (((n1 - 1.0) * v1 + (n2 - 1.0) * v2) / (n1 + n2 - 2.0)).sqrt();
    if pooled == 0.0 {
        return 0.0;
    }
    (mean(treatment) - mean(control)) / pooled
}

/// Hedges' g — Cohen's d with the small-sample bias correction `J`.
pub fn hedges_g(control: &[f64], treatment: &[f64]) -> f64 {
    let n1 = control.len() as f64;
    let n2 = treatment.len() as f64;
    let dof = n1 + n2 - 2.0;
    if dof <= 0.0 {
        return f64::NAN;
    }
    // J ≈ 1 − 3 / (4·df − 1) (Hedges 1981 correction factor).
    let j = 1.0 - 3.0 / (4.0 * dof - 1.0);
    cohens_d(control, treatment) * j
}

/// Cliff's δ ∈ [−1, 1] (Cliff 1993): the rank-based, distribution-free
/// dominance of treatment over control. Positive ⇒ treatment values tend to
/// exceed control values.
pub fn cliffs_delta(control: &[f64], treatment: &[f64]) -> f64 {
    let (n1, n2) = (control.len(), treatment.len());
    if n1 == 0 || n2 == 0 {
        return f64::NAN;
    }
    let mut gt = 0_i64;
    let mut lt = 0_i64;
    for &t in treatment {
        for &c in control {
            match t.partial_cmp(&c) {
                Some(Ordering::Greater) => gt += 1,
                Some(Ordering::Less) => lt += 1,
                _ => {}
            }
        }
    }
    (gt - lt) as f64 / (n1 as f64 * n2 as f64)
}

/// Rank-biserial correlation derived from a Mann-Whitney U for the treatment
/// arm. Equals Cliff's δ; kept as a named estimator for criteria that ask
/// for it explicitly.
pub fn rank_biserial(u_treatment: f64, n_control: usize, n_treatment: usize) -> f64 {
    let denom = n_control as f64 * n_treatment as f64;
    if denom == 0.0 {
        return f64::NAN;
    }
    2.0 * u_treatment / denom - 1.0
}

/// Relative change of the median, treatment vs control: `(med_t − med_c) / |med_c|`.
pub fn relative_change_median(control: &[f64], treatment: &[f64]) -> f64 {
    let mc = median(control);
    let mt = median(treatment);
    if mc == 0.0 {
        return f64::NAN;
    }
    (mt - mc) / mc.abs()
}

// ============================================================================
// Welch's t-test
// ============================================================================

/// Welch's unequal-variance t-test (Welch 1947, Satterthwaite 1946).
/// Difference and Cohen's d are oriented treatment − control. Two- or
/// one-sided per `tail`; the CI is on the mean difference at `ci_level`.
pub fn welch_t_test(
    control: &[f64],
    treatment: &[f64],
    tail: Tail,
    ci_level: f64,
) -> Result<TestResult, StatsError> {
    validate_finite(control)?;
    validate_finite(treatment)?;
    let (n1, n2) = (control.len(), treatment.len());
    if n1 < 2 {
        return Err(StatsError::TooFewSamples { min: 2, got: n1 });
    }
    if n2 < 2 {
        return Err(StatsError::TooFewSamples { min: 2, got: n2 });
    }
    if !(0.0..1.0).contains(&ci_level) && ci_level != 0.0 {
        // ci_level is a confidence like 0.95; must be in (0,1).
    }
    if ci_level <= 0.0 || ci_level >= 1.0 {
        return Err(StatsError::InvalidParam(format!(
            "ci_level must be in (0,1), got {ci_level}"
        )));
    }

    let m1 = mean(control);
    let m2 = mean(treatment);
    let v1 = sample_variance(control);
    let v2 = sample_variance(treatment);
    let se2 = v1 / n1 as f64 + v2 / n2 as f64;
    if se2 <= 0.0 {
        return Err(StatsError::DegenerateVariance);
    }
    let se = se2.sqrt();
    let diff = m2 - m1;
    let t = diff / se;

    // Welch–Satterthwaite effective degrees of freedom.
    let a = v1 / n1 as f64;
    let b = v2 / n2 as f64;
    let df = (a + b) * (a + b) / (a * a / (n1 as f64 - 1.0) + b * b / (n2 as f64 - 1.0));

    let dist = StudentsT::new(0.0, 1.0, df)
        .map_err(|e| StatsError::InvalidParam(format!("StudentsT df={df}: {e}")))?;
    let p_value = match tail {
        Tail::TwoSided => (2.0 * (1.0 - dist.cdf(t.abs()))).clamp(0.0, 1.0),
        Tail::Greater => (1.0 - dist.cdf(t)).clamp(0.0, 1.0),
        Tail::Less => dist.cdf(t).clamp(0.0, 1.0),
    };

    let t_crit = dist.inverse_cdf(1.0 - (1.0 - ci_level) / 2.0);
    let margin = t_crit * se;
    let d = cohens_d(control, treatment);

    let mut notes = Vec::new();
    if n1 < 20 || n2 < 20 {
        notes.push(format!(
            "small sample (n_control={n1}, n_treatment={n2}); Welch p may be unreliable — consider Mann-Whitney"
        ));
    }

    Ok(TestResult {
        kind: TestKind::WelchT,
        tail,
        statistic: t,
        df: Some(df),
        p_value,
        effect_size: Some(d),
        effect_kind: Some(EffectKind::CohensD),
        ci_low: Some(diff - margin),
        ci_high: Some(diff + margin),
        ci_level,
        n_control: n1,
        n_treatment: n2,
        notes,
    })
}

// ============================================================================
// Mann-Whitney U (rank-sum)
// ============================================================================

/// Mann-Whitney U test via the normal approximation with tie + continuity
/// correction (Mann & Whitney 1947). The reported `statistic` is
/// `U_treatment` (the count of treatment-beats-control pairs, ties counted
/// as ½); the effect size is Cliff's δ. Suited to the non-normal,
/// heavy-tailed distributions typical of latency/throughput benchmarks.
pub fn mann_whitney_u(
    control: &[f64],
    treatment: &[f64],
    tail: Tail,
) -> Result<TestResult, StatsError> {
    validate_finite(control)?;
    validate_finite(treatment)?;
    let (n1, n2) = (control.len(), treatment.len());
    if n1 < 1 || n2 < 1 {
        return Err(StatsError::TooFewSamples {
            min: 1,
            got: n1.min(n2),
        });
    }

    // Pool with treatment first so the first n2 ranks belong to treatment.
    let mut pooled = Vec::with_capacity(n1 + n2);
    pooled.extend_from_slice(treatment);
    pooled.extend_from_slice(control);
    let (ranks, tie_term) = average_ranks(&pooled);

    let r_treatment: f64 = ranks[..n2].iter().sum();
    let nt = n2 as f64;
    let nc = n1 as f64;
    let big_n = nt + nc;
    let u_treatment = r_treatment - nt * (nt + 1.0) / 2.0;

    let mu = nt * nc / 2.0;
    // Variance with tie correction (Mann-Whitney normal approximation).
    let var = (nt * nc / 12.0) * ((big_n + 1.0) - tie_term / (big_n * (big_n - 1.0)));
    let delta = cliffs_delta(control, treatment);

    let mut notes = Vec::new();
    if n1 < 8 || n2 < 8 {
        notes.push(format!(
            "very small sample (n_control={n1}, n_treatment={n2}); normal-approximation MWU p is approximate"
        ));
    }

    if var <= 0.0 {
        // All values identical across both arms: no evidence of a difference.
        return Ok(TestResult {
            kind: TestKind::MannWhitneyU,
            tail,
            statistic: u_treatment,
            df: None,
            p_value: 1.0,
            effect_size: Some(delta),
            effect_kind: Some(EffectKind::CliffsDelta),
            ci_low: None,
            ci_high: None,
            ci_level: 0.0,
            n_control: n1,
            n_treatment: n2,
            notes,
        });
    }
    let sigma = var.sqrt();
    // Continuity correction toward the mean.
    let cc = if u_treatment > mu {
        -0.5
    } else if u_treatment < mu {
        0.5
    } else {
        0.0
    };
    let z = (u_treatment - mu + cc) / sigma;
    let p_value = one_sided_normal_p(z, tail);

    Ok(TestResult {
        kind: TestKind::MannWhitneyU,
        tail,
        statistic: u_treatment,
        df: None,
        p_value,
        effect_size: Some(delta),
        effect_kind: Some(EffectKind::CliffsDelta),
        ci_low: None,
        ci_high: None,
        ci_level: 0.0,
        n_control: n1,
        n_treatment: n2,
        notes,
    })
}

// ============================================================================
// Wilcoxon signed-rank (paired)
// ============================================================================

/// Wilcoxon signed-rank test for paired samples (Wilcoxon 1945) via the
/// normal approximation with tie + continuity correction. `control` and
/// `treatment` must be equal-length, paired observations (e.g. the same
/// files' complexity before vs after a refactor). The effect size is the
/// matched-pairs rank-biserial correlation `(W⁺ − W⁻)/(W⁺ + W⁻)`.
pub fn wilcoxon_signed_rank(
    control: &[f64],
    treatment: &[f64],
    tail: Tail,
) -> Result<TestResult, StatsError> {
    validate_finite(control)?;
    validate_finite(treatment)?;
    if control.len() != treatment.len() {
        return Err(StatsError::LengthMismatch {
            n_control: control.len(),
            n_treatment: treatment.len(),
        });
    }
    // Non-zero paired differences, oriented treatment − control.
    let diffs: Vec<f64> = control
        .iter()
        .zip(treatment)
        .map(|(c, t)| t - c)
        .filter(|d| *d != 0.0)
        .collect();
    let n = diffs.len();
    if n < 1 {
        return Err(StatsError::TooFewSamples { min: 1, got: 0 });
    }
    let abs: Vec<f64> = diffs.iter().map(|d| d.abs()).collect();
    let (ranks, tie_term) = average_ranks(&abs);
    let mut w_plus = 0.0_f64;
    let mut w_minus = 0.0_f64;
    for (d, r) in diffs.iter().zip(&ranks) {
        if *d > 0.0 {
            w_plus += r;
        } else {
            w_minus += r;
        }
    }
    let nf = n as f64;
    let mu = nf * (nf + 1.0) / 4.0;
    let var = nf * (nf + 1.0) * (2.0 * nf + 1.0) / 24.0 - tie_term / 48.0;
    let total = w_plus + w_minus;
    let effect = if total > 0.0 {
        (w_plus - w_minus) / total
    } else {
        0.0
    };

    let mut notes = Vec::new();
    if n < 10 {
        notes.push(format!(
            "few non-zero pairs (n={n}); signed-rank normal approximation is rough"
        ));
    }

    if var <= 0.0 {
        return Ok(TestResult {
            kind: TestKind::WilcoxonSignedRank,
            tail,
            statistic: w_plus,
            df: None,
            p_value: 1.0,
            effect_size: Some(effect),
            effect_kind: Some(EffectKind::RankBiserial),
            ci_low: None,
            ci_high: None,
            ci_level: 0.0,
            n_control: control.len(),
            n_treatment: treatment.len(),
            notes,
        });
    }
    let sigma = var.sqrt();
    let cc = if w_plus > mu {
        -0.5
    } else if w_plus < mu {
        0.5
    } else {
        0.0
    };
    let z = (w_plus - mu + cc) / sigma;
    let p_value = one_sided_normal_p(z, tail);

    Ok(TestResult {
        kind: TestKind::WilcoxonSignedRank,
        tail,
        statistic: w_plus,
        df: None,
        p_value,
        effect_size: Some(effect),
        effect_kind: Some(EffectKind::RankBiserial),
        ci_low: None,
        ci_high: None,
        ci_level: 0.0,
        n_control: control.len(),
        n_treatment: treatment.len(),
        notes,
    })
}

// ============================================================================
// Bootstrap confidence intervals
// ============================================================================

/// Which point estimate the bootstrap resamples.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Estimand {
    Mean,
    Median,
}

/// CI construction method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootMethod {
    Percentile,
    /// Bias-corrected and accelerated (DiCiccio-Efron 1996).
    Bca,
}

/// Bootstrap configuration. `seed` makes a run reproducible (recorded into
/// the experiment so a decision can be re-derived).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct BootstrapConfig {
    pub resamples: usize,
    pub ci_level: f64,
    pub method: BootMethod,
    pub seed: u64,
}

impl Default for BootstrapConfig {
    fn default() -> Self {
        Self {
            resamples: 10_000,
            ci_level: 0.95,
            method: BootMethod::Bca,
            seed: 0,
        }
    }
}

#[inline]
fn point_estimate(samples: &[f64], estimand: Estimand) -> f64 {
    match estimand {
        Estimand::Mean => mean(samples),
        Estimand::Median => median(samples),
    }
}

/// xorshift64* — a tiny, fast, deterministic PRNG. We only need uniform
/// indices for resampling and want exact reproducibility from `seed` without
/// depending on a specific `rand` distribution's stream stability.
struct XorShift64(u64);
impl XorShift64 {
    fn new(seed: u64) -> Self {
        // Avoid the all-zero state.
        XorShift64(seed ^ 0x9E37_79B9_7F4A_7C15)
    }
    #[inline]
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    #[inline]
    fn index(&mut self, len: usize) -> usize {
        (self.next_u64() % len as u64) as usize
    }
}

fn bootstrap_diff(
    control: &[f64],
    treatment: &[f64],
    estimand: Estimand,
    cfg: &BootstrapConfig,
) -> Result<TestResult, StatsError> {
    validate_finite(control)?;
    validate_finite(treatment)?;
    let (n1, n2) = (control.len(), treatment.len());
    if n1 < 2 || n2 < 2 {
        return Err(StatsError::TooFewSamples {
            min: 2,
            got: n1.min(n2),
        });
    }
    if cfg.resamples < 100 {
        return Err(StatsError::InvalidParam(format!(
            "resamples must be >= 100, got {}",
            cfg.resamples
        )));
    }
    if cfg.ci_level <= 0.0 || cfg.ci_level >= 1.0 {
        return Err(StatsError::InvalidParam(format!(
            "ci_level must be in (0,1), got {}",
            cfg.ci_level
        )));
    }

    let observed = point_estimate(treatment, estimand) - point_estimate(control, estimand);

    let mut rng = XorShift64::new(cfg.seed);
    let mut estimates = Vec::with_capacity(cfg.resamples);
    let mut buf_c = vec![0.0_f64; n1];
    let mut buf_t = vec![0.0_f64; n2];
    let mut below = 0_usize; // # resamples strictly below 0 (for ASL)
    let mut above = 0_usize;
    for _ in 0..cfg.resamples {
        for slot in buf_c.iter_mut() {
            *slot = control[rng.index(n1)];
        }
        for slot in buf_t.iter_mut() {
            *slot = treatment[rng.index(n2)];
        }
        let est = point_estimate(&buf_t, estimand) - point_estimate(&buf_c, estimand);
        if est < 0.0 {
            below += 1;
        } else if est > 0.0 {
            above += 1;
        }
        estimates.push(est);
    }
    estimates.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));

    let alpha = 1.0 - cfg.ci_level;
    let (lo_q, hi_q) = match cfg.method {
        BootMethod::Percentile => (alpha / 2.0, 1.0 - alpha / 2.0),
        BootMethod::Bca => bca_quantiles(&estimates, observed, control, treatment, estimand, alpha),
    };
    let ci_low = percentile_sorted(&estimates, lo_q);
    let ci_high = percentile_sorted(&estimates, hi_q);

    // Two-sided achieved significance level: twice the smaller tail mass at 0.
    let b = cfg.resamples as f64;
    let asl = (2.0 * (below.min(above) as f64 + 0.0) / b).clamp(0.0, 1.0);

    let kind = match estimand {
        Estimand::Mean => TestKind::BootstrapDiffMeans,
        Estimand::Median => TestKind::BootstrapDiffMedians,
    };
    Ok(TestResult {
        kind,
        tail: Tail::TwoSided,
        statistic: observed,
        df: None,
        p_value: asl,
        effect_size: Some(observed),
        effect_kind: Some(EffectKind::RelativeChange),
        ci_low: Some(ci_low),
        ci_high: Some(ci_high),
        ci_level: cfg.ci_level,
        n_control: n1,
        n_treatment: n2,
        notes: Vec::new(),
    })
}

/// BCa adjusted percentile points (DiCiccio-Efron 1996). Bias-correction
/// `ẑ₀` from the resample mass below the observed estimate; acceleration `â`
/// from the pooled jackknife skewness of the two-sample difference.
fn bca_quantiles(
    sorted_estimates: &[f64],
    observed: f64,
    control: &[f64],
    treatment: &[f64],
    estimand: Estimand,
    alpha: f64,
) -> (f64, f64) {
    let b = sorted_estimates.len() as f64;
    let n_below = sorted_estimates.iter().filter(|&&e| e < observed).count() as f64;
    let prop = (n_below / b).clamp(1.0 / (b + 1.0), b / (b + 1.0));
    let normal = standard_normal();
    let z0 = normal.inverse_cdf(prop);

    // Jackknife over the pooled observations: drop one at a time from
    // whichever arm it belongs to, recompute the difference.
    let m_c = point_estimate(control, estimand);
    let m_t = point_estimate(treatment, estimand);
    let mut jack = Vec::with_capacity(control.len() + treatment.len());
    let mut scratch: Vec<f64> = Vec::with_capacity(treatment.len().max(control.len()));
    for i in 0..control.len() {
        scratch.clear();
        scratch.extend(
            control
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, v)| *v),
        );
        jack.push(m_t - point_estimate(&scratch, estimand));
    }
    for i in 0..treatment.len() {
        scratch.clear();
        scratch.extend(
            treatment
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, v)| *v),
        );
        jack.push(point_estimate(&scratch, estimand) - m_c);
    }
    let jbar = mean(&jack);
    let mut num = 0.0_f64;
    let mut den = 0.0_f64;
    for j in &jack {
        let d = jbar - j;
        num += d * d * d;
        den += d * d;
    }
    let acc = if den > 0.0 {
        num / (6.0 * den.powf(1.5))
    } else {
        0.0
    };

    let z_lo = normal.inverse_cdf(alpha / 2.0);
    let z_hi = normal.inverse_cdf(1.0 - alpha / 2.0);
    let adjust = |z: f64| -> f64 {
        let v = z0 + (z0 + z) / (1.0 - acc * (z0 + z));
        normal.cdf(v).clamp(0.0, 1.0)
    };
    (adjust(z_lo), adjust(z_hi))
}

/// Bootstrap CI on the difference of means.
pub fn bootstrap_diff_means(
    control: &[f64],
    treatment: &[f64],
    cfg: &BootstrapConfig,
) -> Result<TestResult, StatsError> {
    bootstrap_diff(control, treatment, Estimand::Mean, cfg)
}

/// Bootstrap CI on the difference of medians (robust to outliers/heavy tails).
pub fn bootstrap_diff_medians(
    control: &[f64],
    treatment: &[f64],
    cfg: &BootstrapConfig,
) -> Result<TestResult, StatsError> {
    bootstrap_diff(control, treatment, Estimand::Median, cfg)
}

// ============================================================================
// Equivalence testing (TOST)
// ============================================================================

/// Two One-Sided Tests for equivalence (Schuirmann 1987). Concludes the
/// treatment is equivalent to the control — within `(low, high)` on the
/// treatment − control difference — iff both one-sided Welch tests reject at
/// `alpha`, equivalently the `1 − 2·alpha` CI of the difference lies entirely
/// inside the margin. Use for "no regression" / "preserves performance"
/// claims, which a non-significant two-sided t-test cannot establish.
pub fn tost_equivalence(
    control: &[f64],
    treatment: &[f64],
    low: f64,
    high: f64,
    alpha: f64,
) -> Result<TestResult, StatsError> {
    validate_finite(control)?;
    validate_finite(treatment)?;
    if low >= high {
        return Err(StatsError::InvalidParam(format!(
            "equivalence margin must have low < high, got ({low}, {high})"
        )));
    }
    if alpha <= 0.0 || alpha >= 0.5 {
        return Err(StatsError::InvalidParam(format!(
            "alpha must be in (0, 0.5), got {alpha}"
        )));
    }
    let (n1, n2) = (control.len(), treatment.len());
    if n1 < 2 || n2 < 2 {
        return Err(StatsError::TooFewSamples {
            min: 2,
            got: n1.min(n2),
        });
    }
    let m1 = mean(control);
    let m2 = mean(treatment);
    let v1 = sample_variance(control);
    let v2 = sample_variance(treatment);
    let se2 = v1 / n1 as f64 + v2 / n2 as f64;
    if se2 <= 0.0 {
        return Err(StatsError::DegenerateVariance);
    }
    let se = se2.sqrt();
    let diff = m2 - m1;
    let a = v1 / n1 as f64;
    let b = v2 / n2 as f64;
    let df = (a + b) * (a + b) / (a * a / (n1 as f64 - 1.0) + b * b / (n2 as f64 - 1.0));
    let dist = StudentsT::new(0.0, 1.0, df)
        .map_err(|e| StatsError::InvalidParam(format!("StudentsT df={df}: {e}")))?;

    // H₀₁: diff ≤ low (reject if diff sufficiently above low).
    let t_lower = (diff - low) / se;
    let p_lower = (1.0 - dist.cdf(t_lower)).clamp(0.0, 1.0);
    // H₀₂: diff ≥ high (reject if diff sufficiently below high).
    let t_upper = (diff - high) / se;
    let p_upper = dist.cdf(t_upper).clamp(0.0, 1.0);

    let tost_p = p_lower.max(p_upper);
    // 1 − 2α CI on the difference.
    let t_crit = dist.inverse_cdf(1.0 - alpha);
    let ci_low = diff - t_crit * se;
    let ci_high = diff + t_crit * se;

    Ok(TestResult {
        kind: TestKind::Tost,
        tail: Tail::TwoSided,
        statistic: diff,
        df: Some(df),
        p_value: tost_p,
        effect_size: Some(diff),
        effect_kind: Some(EffectKind::RelativeChange),
        ci_low: Some(ci_low),
        ci_high: Some(ci_high),
        ci_level: 1.0 - 2.0 * alpha,
        n_control: n1,
        n_treatment: n2,
        notes: vec![format!(
            "equivalence margin ({low}, {high}); equivalent iff {:.4} CI ⊂ margin",
            1.0 - 2.0 * alpha
        )],
    })
}

// ============================================================================
// Normality assessment + test recommendation
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalityReport {
    pub statistic: f64,
    pub p_value: f64,
    /// True when the sample is consistent with normality at the 0.05 level
    /// (i.e. we fail to reject normality).
    pub is_normalish: bool,
    pub note: String,
}

/// Anderson-Darling test for normality with estimated mean/variance
/// (Stephens 1974). Uses only the normal CDF. Returns an approximate p-value
/// from Stephens' piecewise fit to the modified statistic `A²*`.
pub fn anderson_darling(sample: &[f64]) -> Result<NormalityReport, StatsError> {
    validate_finite(sample)?;
    let n = sample.len();
    if n < 8 {
        return Err(StatsError::TooFewSamples { min: 8, got: n });
    }
    let m = mean(sample);
    let sd = sample_variance(sample).sqrt();
    if sd == 0.0 {
        return Err(StatsError::DegenerateVariance);
    }
    let normal = standard_normal();
    let sorted = sorted_copy(sample);
    let nf = n as f64;
    let mut s = 0.0_f64;
    for (i, &x) in sorted.iter().enumerate() {
        let z_lo = (x - m) / sd;
        let z_hi = (sorted[n - 1 - i] - m) / sd;
        let f_lo = normal.cdf(z_lo).clamp(1e-12, 1.0 - 1e-12);
        let f_hi = normal.cdf(z_hi).clamp(1e-12, 1.0 - 1e-12);
        let coef = (2 * (i + 1) - 1) as f64;
        s += coef * (f_lo.ln() + (1.0 - f_hi).ln());
    }
    let a2 = -nf - s / nf;
    // Modification for estimated parameters (Stephens 1974).
    let a2_star = a2 * (1.0 + 0.75 / nf + 2.25 / (nf * nf));
    // Stephens' piecewise p-value approximation.
    let p = if a2_star >= 0.6 {
        (1.2937 - 5.709 * a2_star + 0.0186 * a2_star * a2_star).exp()
    } else if a2_star >= 0.34 {
        (0.9177 - 4.279 * a2_star - 1.38 * a2_star * a2_star).exp()
    } else if a2_star >= 0.2 {
        1.0 - (-8.318 + 42.796 * a2_star - 59.938 * a2_star * a2_star).exp()
    } else {
        1.0 - (-13.436 + 101.14 * a2_star - 223.73 * a2_star * a2_star).exp()
    };
    let p = p.clamp(0.0, 1.0);
    Ok(NormalityReport {
        statistic: a2_star,
        p_value: p,
        is_normalish: p > 0.05,
        note: "Anderson-Darling (Stephens 1974), parameters estimated".to_string(),
    })
}

/// D'Agostino-Pearson K² omnibus normality test (skewness + kurtosis),
/// scipy's `normaltest`. Needs the normal and χ²(2) CDFs only. Requires
/// n ≥ 20 for the transforms to be meaningful.
pub fn dagostino_pearson(sample: &[f64]) -> Result<NormalityReport, StatsError> {
    validate_finite(sample)?;
    let n = sample.len();
    if n < 20 {
        return Err(StatsError::TooFewSamples { min: 20, got: n });
    }
    let nf = n as f64;
    let m = mean(sample);
    let mut m2 = 0.0;
    let mut m3 = 0.0;
    let mut m4 = 0.0;
    for &x in sample {
        let d = x - m;
        m2 += d * d;
        m3 += d * d * d;
        m4 += d * d * d * d;
    }
    m2 /= nf;
    m3 /= nf;
    m4 /= nf;
    if m2 == 0.0 {
        return Err(StatsError::DegenerateVariance);
    }
    // Sample skewness g1 and kurtosis g2 (Fisher).
    let g1 = m3 / m2.powf(1.5);
    let g2 = m4 / (m2 * m2) - 3.0;

    // D'Agostino's transform for skewness.
    let y = g1 * ((nf + 1.0) * (nf + 3.0) / (6.0 * (nf - 2.0))).sqrt();
    let beta2 = 3.0 * (nf * nf + 27.0 * nf - 70.0) * (nf + 1.0) * (nf + 3.0)
        / ((nf - 2.0) * (nf + 5.0) * (nf + 7.0) * (nf + 9.0));
    let w2 = -1.0 + (2.0 * (beta2 - 1.0)).sqrt();
    let w = w2.sqrt();
    let delta = 1.0 / w.ln().sqrt();
    let a = (2.0 / (w2 - 1.0)).sqrt();
    let z_skew = delta * (y / a + ((y / a).powi(2) + 1.0).sqrt()).ln();

    // Anscombe-Glynn transform for kurtosis.
    let e_g2 = -6.0 / (nf + 1.0);
    let var_g2 =
        24.0 * nf * (nf - 2.0) * (nf - 3.0) / ((nf + 1.0) * (nf + 1.0) * (nf + 3.0) * (nf + 5.0));
    let x_k = (g2 - e_g2) / var_g2.sqrt();
    let sqrt_beta1 = 6.0 * (nf * nf - 5.0 * nf + 2.0) / ((nf + 7.0) * (nf + 9.0))
        * (6.0 * (nf + 3.0) * (nf + 5.0) / (nf * (nf - 2.0) * (nf - 3.0))).sqrt();
    let a_k = 6.0
        + 8.0 / sqrt_beta1 * (2.0 / sqrt_beta1 + (1.0 + 4.0 / (sqrt_beta1 * sqrt_beta1)).sqrt());
    let term = (1.0 - 2.0 / a_k) / (1.0 + x_k * (2.0 / (a_k - 4.0)).sqrt());
    let z_kurt = ((1.0 - 2.0 / (9.0 * a_k)) - term.cbrt()) / (2.0 / (9.0 * a_k)).sqrt();

    let k2 = z_skew * z_skew + z_kurt * z_kurt;
    let chi2 = ChiSquared::new(2.0).map_err(|e| StatsError::InvalidParam(format!("chi2: {e}")))?;
    let p = (1.0 - chi2.cdf(k2)).clamp(0.0, 1.0);
    Ok(NormalityReport {
        statistic: k2,
        p_value: p,
        is_normalish: p > 0.05,
        note: "D'Agostino-Pearson K² omnibus".to_string(),
    })
}

/// Which two-sample test the data recommend, given normality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecommendedTest {
    Welch,
    MannWhitney,
}

/// Recommend Welch (both arms approximately normal and adequately sized) or
/// Mann-Whitney otherwise. This is advisory — it never overrides a
/// pre-registered acceptance criterion — and returns warnings to surface to
/// the operator. Falls back to Anderson-Darling when an arm is too small for
/// the K² omnibus.
pub fn recommend_two_sample_test(
    control: &[f64],
    treatment: &[f64],
) -> (RecommendedTest, Vec<String>) {
    let mut notes = Vec::new();
    let normality = |s: &[f64], label: &str, notes: &mut Vec<String>| -> bool {
        let report = if s.len() >= 20 {
            dagostino_pearson(s)
        } else {
            anderson_darling(s)
        };
        match report {
            Ok(r) => {
                if !r.is_normalish {
                    notes.push(format!(
                        "{label} arm departs from normality ({}, p={:.4})",
                        r.note, r.p_value
                    ));
                }
                r.is_normalish
            }
            Err(_) => {
                notes.push(format!(
                    "{label} arm too small to assess normality; assuming non-normal"
                ));
                false
            }
        }
    };
    if control.len() < 20 || treatment.len() < 20 {
        notes.push(format!(
            "small samples (n_control={}, n_treatment={}); prefer the non-parametric test",
            control.len(),
            treatment.len()
        ));
    }
    let c_normal = normality(control, "control", &mut notes);
    let t_normal = normality(treatment, "treatment", &mut notes);
    if c_normal && t_normal && control.len() >= 20 && treatment.len() >= 20 {
        (RecommendedTest::Welch, notes)
    } else {
        (RecommendedTest::MannWhitney, notes)
    }
}

// ============================================================================
// Multiple-comparison correction
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Correction {
    None,
    Bonferroni,
    /// Benjamini-Hochberg false-discovery-rate step-up (1995).
    BenjaminiHochberg,
}

/// Adjust a vector of p-values, returning adjusted p-values aligned to the
/// input order. Bonferroni controls FWER; Benjamini-Hochberg controls FDR
/// (the recommended default when one experiment compares many correlated
/// metrics — Bonferroni is needlessly punishing there).
pub fn adjust_pvalues(p: &[f64], method: Correction) -> Vec<f64> {
    let m = p.len();
    if m == 0 {
        return Vec::new();
    }
    match method {
        Correction::None => p.to_vec(),
        Correction::Bonferroni => p.iter().map(|&v| (v * m as f64).min(1.0)).collect(),
        Correction::BenjaminiHochberg => {
            // Order p ascending, apply (m/rank) factor, enforce monotonicity.
            let mut order: Vec<usize> = (0..m).collect();
            order.sort_by(|&a, &b| p[a].partial_cmp(&p[b]).unwrap_or(Ordering::Equal));
            let mut adjusted = vec![0.0_f64; m];
            let mut prev = 1.0_f64;
            for k in (0..m).rev() {
                let idx = order[k];
                let rank = (k + 1) as f64;
                let val = (p[idx] * m as f64 / rank).min(1.0);
                prev = prev.min(val);
                adjusted[idx] = prev;
            }
            adjusted
        }
    }
}

// ============================================================================
// Power / sample-size planning
// ============================================================================

/// Required samples per arm to detect a standardized effect `effect_d` (Cohen's
/// d) at significance `alpha` with the given `power`, for a two-sample
/// comparison (Cohen 1988 normal approximation, with a +1 small-sample bump).
/// Returns at least 2.
pub fn required_n_per_arm(effect_d: f64, alpha: f64, power: f64, tail: Tail) -> usize {
    if effect_d == 0.0 || !effect_d.is_finite() {
        return usize::MAX; // an effect of zero needs infinite samples
    }
    if alpha <= 0.0 || alpha >= 1.0 || power <= 0.0 || power >= 1.0 {
        return 2;
    }
    let normal = standard_normal();
    let z_alpha = match tail {
        Tail::TwoSided => normal.inverse_cdf(1.0 - alpha / 2.0),
        _ => normal.inverse_cdf(1.0 - alpha),
    };
    let z_beta = normal.inverse_cdf(power);
    let n = 2.0 * ((z_alpha + z_beta) / effect_d).powi(2) + 1.0;
    (n.ceil() as usize).max(2)
}

// ============================================================================
// Exact binomial test
// ============================================================================

/// Exact (or, for very large `n`, normal-approximated) binomial test of the
/// observed `successes` out of `n` trials against a null success probability
/// `p0`. Returns the p-value for the requested [`Tail`].
///
/// ## Two-sided convention — the doubling rule
///
/// The two-sided p-value uses the **doubling rule** `min(1, 2·min(P_left,
/// P_right))` where `P_left = P(X ≤ successes)` and `P_right = P(X ≥ successes)`
/// under `X ~ Binomial(n, p0)`. This is the convention R's `binom.test`
/// documents as the "central" / minimum-likelihood-free variant and is what
/// McNemar's exact test (where `p0 = 0.5`) calls for. It is simpler and more
/// numerically robust than the "sum of all outcomes no more probable than the
/// observed" (`minlike`) rule, and identical to it for the symmetric
/// `p0 = 0.5` case that drives McNemar. Note the doubled smaller tail double-
/// counts the shared point mass at `successes`, so for a symmetric null the
/// p-value correctly saturates to `1.0` when `successes` sits at the median.
///
/// ## Exact vs. normal approximation
///
/// For `n ≤ 1000` the tails are summed exactly from the binomial PMF, evaluated
/// in log-space (`ln_binom_pmf`) so the binomial coefficient never overflows and
/// small probabilities keep full relative precision. This is the regime
/// McNemar's exact path (`n ≤ 25`) depends on, so it must be exact there. For
/// `n > 1000` an exact summation over up to a thousand-plus terms buys nothing
/// over the central-limit regime and risks needless work, so we fall back to the
/// normal approximation with a continuity correction (mean `n·p0`, variance
/// `n·p0·(1−p0)`); the threshold is documented here and pinned by a unit test.
///
/// ## Errors
///
/// Returns [`StatsError::InvalidParam`] when `successes > n` or `p0` is outside
/// the open interval `(0, 1)` (the test statistic is undefined at the
/// boundaries, where the binomial is degenerate).
pub fn exact_binomial_test(successes: u64, n: u64, p0: f64, tail: Tail) -> Result<f64, StatsError> {
    if successes > n {
        return Err(StatsError::InvalidParam(format!(
            "successes ({successes}) must be <= n ({n})"
        )));
    }
    if !(p0 > 0.0 && p0 < 1.0) {
        return Err(StatsError::InvalidParam(format!(
            "p0 must be in (0,1), got {p0}"
        )));
    }

    // Degenerate trial count: zero trials means the only outcome is 0 successes,
    // which is exactly what was observed — no evidence against H0.
    if n == 0 {
        return Ok(1.0);
    }

    // Large-n normal approximation with continuity correction.
    const EXACT_MAX_N: u64 = 1000;
    if n > EXACT_MAX_N {
        let nf = n as f64;
        let mu = nf * p0;
        let sigma = (nf * p0 * (1.0 - p0)).sqrt();
        if sigma <= 0.0 {
            // Cannot happen for p0 ∈ (0,1), n ≥ 1, but stay total.
            return Ok(1.0);
        }
        let k = successes as f64;
        return Ok(match tail {
            // P(X ≥ k) ≈ 1 − Φ((k − 0.5 − μ)/σ).
            Tail::Greater => {
                let z = (k - 0.5 - mu) / sigma;
                one_sided_normal_p(z, Tail::Greater)
            }
            // P(X ≤ k) ≈ Φ((k + 0.5 − μ)/σ).
            Tail::Less => {
                let z = (k + 0.5 - mu) / sigma;
                one_sided_normal_p(z, Tail::Less)
            }
            Tail::TwoSided => {
                let z_left = (k + 0.5 - mu) / sigma; // for P(X ≤ k)
                let z_right = (k - 0.5 - mu) / sigma; // for P(X ≥ k)
                let p_left = standard_normal().cdf(z_left).clamp(0.0, 1.0);
                let p_right = (1.0 - standard_normal().cdf(z_right)).clamp(0.0, 1.0);
                (2.0 * p_left.min(p_right)).min(1.0)
            }
        });
    }

    // Exact summation from the binomial PMF (log-space for numerical safety).
    let n_usize = n as usize;
    let s = successes as usize;
    // P(X ≤ successes): sum the lower s+1 PMF terms.
    let p_left: f64 = (0..=s).map(|k| ln_binom_pmf(k, n_usize, p0).exp()).sum();
    // P(X ≥ successes): sum the upper (n−s+1) PMF terms.
    let p_right: f64 = (s..=n_usize)
        .map(|k| ln_binom_pmf(k, n_usize, p0).exp())
        .sum();

    let p = match tail {
        Tail::Greater => p_right,
        Tail::Less => p_left,
        // Doubling rule (see the doc comment): saturates to 1.0 at the median.
        Tail::TwoSided => (2.0 * p_left.min(p_right)).min(1.0),
    };
    Ok(p.clamp(0.0, 1.0))
}

/// Natural log of the binomial PMF `C(n,k)·pᵏ·(1−p)ⁿ⁻ᵏ`, computed via
/// `ln_gamma` so the coefficient never overflows for the `n ≤ 1000` exact path.
/// `ln Γ(m+1) = ln(m!)`, so `ln C(n,k) = lnΓ(n+1) − lnΓ(k+1) − lnΓ(n−k+1)`.
fn ln_binom_pmf(k: usize, n: usize, p: f64) -> f64 {
    use statrs::function::gamma::ln_gamma;
    let nf = n as f64;
    let kf = k as f64;
    let ln_choose = ln_gamma(nf + 1.0) - ln_gamma(kf + 1.0) - ln_gamma(nf - kf + 1.0);
    ln_choose + kf * p.ln() + (nf - kf) * (1.0 - p).ln()
}

// ============================================================================
// McNemar's test (paired binary outcomes)
// ============================================================================

/// Result of [`mcnemar_test`]: McNemar's paired test for two binary
/// classifiers scored on the **same** test cases.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McNemarResult {
    /// On the χ² path: the continuity-corrected χ² statistic. On the exact
    /// path: the discordant imbalance `|b − c|` (the χ² statistic is not
    /// computed there, so this carries the magnitude of disagreement instead).
    pub statistic: f64,
    /// Two-sided p-value.
    pub p_value: f64,
    /// Number of discordant pairs, `b + c`.
    pub n_discordant: u64,
    /// `c − b` as f64 — the net count of discordant pairs favoring the
    /// treatment (positive ⇒ treatment fixed more cases than it broke).
    pub effect_treatment_minus_control: f64,
    /// True when the exact-binomial path was used (`n_discordant ≤ 25`).
    pub exact: bool,
}

/// McNemar's test (McNemar 1947) for two binary classifiers evaluated on a
/// **paired** sample — the same test cases scored by a control arm and a
/// treatment arm.
///
/// ## Why not Welch / two-proportion tests here
///
/// When both arms are scored on the *same* cases, the per-arm accuracies are
/// **dependent**: a Welch t-test (or an unpaired two-proportion z-test) ignores
/// that pairing and is statistically wrong — it answers the wrong question and
/// is anticonservative. McNemar conditions on the **discordant** pairs (cases
/// the two arms disagree on) and asks only whether disagreements split
/// symmetrically. The concordant cells (`both_correct`, `both_wrong`) carry no
/// information about a difference and drop out of the statistic entirely.
///
/// ## The 2×2 table
///
/// ```text
///                          treatment correct   treatment wrong
///   control correct        a = both_correct    b = control_only_correct
///   control wrong          c = treatment_only  d = both_wrong
///                              _correct
/// ```
///
/// Under H₀ (no difference in error rates) the `n = b + c` discordant pairs
/// split as `b ~ Binomial(n, ½)`.
///
/// ## Exact vs. χ² (continuity-corrected) path
///
/// - `n == 0`: no discordant pairs, hence no evidence of a difference — returns
///   `p = 1.0`, `statistic = 0.0`, `exact = true`.
/// - `n ≤ 25`: the **exact binomial** two-sided test (the χ² approximation is
///   unreliable for few discordant pairs). `statistic` is reported as the
///   discordant imbalance `|b − c|` (the χ² value is not meaningful here).
/// - `n > 25`: Edwards' **continuity-corrected** χ² with one degree of freedom,
///   `statistic = (max(|b − c| − 1, 0))² / n`, and `p_value` is its χ²(1) upper
///   tail. The `max(·, 0)` clamp keeps the corrected term non-negative when
///   `|b − c| < 1` (i.e. `b == c`), which would otherwise square a negative.
///
/// The effect is oriented treatment − control: `effect_treatment_minus_control
/// = c − b`.
///
/// ## References
///
/// - McNemar, Q. (1947). *Psychometrika* 12(2). (the paired test)
/// - Edwards, A. L. (1948). *Psychometrika* 13(3). (continuity correction)
pub fn mcnemar_test(
    both_correct: u64,
    control_only_correct: u64,
    treatment_only_correct: u64,
    both_wrong: u64,
) -> Result<McNemarResult, StatsError> {
    // Concordant cells carry no information about a difference; named for the
    // table documentation above and to make the "ignored except for context"
    // contract explicit.
    let _a = both_correct;
    let _d = both_wrong;
    let b = control_only_correct;
    let c = treatment_only_correct;

    let n = b + c; // discordant pairs
    let effect = c as f64 - b as f64;

    // No discordant pairs → no evidence of any difference.
    if n == 0 {
        return Ok(McNemarResult {
            statistic: 0.0,
            p_value: 1.0,
            n_discordant: 0,
            effect_treatment_minus_control: 0.0,
            exact: true,
        });
    }

    // Exact path for small discordant counts (the χ² approximation is unreliable
    // for few discordant pairs).
    const EXACT_MAX_DISCORDANT: u64 = 25;
    if n <= EXACT_MAX_DISCORDANT {
        // Under H0, b ~ Binomial(n, 0.5). The two-sided doubling rule applied at
        // the smaller cell (min(b,c) ≤ n/2, so its lower tail is the smaller
        // one) yields exactly min(1, 2·P(X ≤ min(b,c))).
        let p_value = exact_binomial_test(b.min(c), n, 0.5, Tail::TwoSided)?;
        return Ok(McNemarResult {
            // The exact path has no χ² statistic; report the discordant
            // imbalance |b − c| as the magnitude of disagreement.
            statistic: (b as f64 - c as f64).abs(),
            p_value,
            n_discordant: n,
            effect_treatment_minus_control: effect,
            exact: true,
        });
    }

    // χ² path with Edwards' continuity correction.
    let diff = (b as f64 - c as f64).abs();
    let corrected = (diff - 1.0).max(0.0); // clamp so it can't go negative
    let statistic = corrected * corrected / n as f64;
    let chi2 =
        ChiSquared::new(1.0).map_err(|e| StatsError::InvalidParam(format!("chi2 df=1: {e}")))?;
    let p_value = (1.0 - chi2.cdf(statistic)).clamp(0.0, 1.0);

    Ok(McNemarResult {
        statistic,
        p_value,
        n_discordant: n,
        effect_treatment_minus_control: effect,
        exact: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tolerances: golden values are from SciPy 1.x / R 4.x on the same vectors.
    fn approx(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() <= tol
    }

    const A: [f64; 10] = [5.1, 4.9, 5.3, 5.0, 5.2, 4.8, 5.1, 5.0, 4.95, 5.05];
    const B: [f64; 10] = [5.5, 5.7, 5.4, 5.6, 5.8, 5.3, 5.6, 5.5, 5.65, 5.45];

    #[test]
    fn welford_matches_two_pass() {
        let (n, m, m2) = welford(&A);
        assert_eq!(n, 10);
        let two_pass_mean: f64 = A.iter().sum::<f64>() / 10.0;
        assert!(approx(m, two_pass_mean, 1e-12));
        let two_pass_var: f64 = A.iter().map(|x| (x - two_pass_mean).powi(2)).sum::<f64>() / 9.0;
        assert!(approx(m2 / 9.0, two_pass_var, 1e-12));
    }

    #[test]
    fn welch_detects_clear_difference() {
        // B is ~0.5 above A with similar spread → highly significant.
        let r = welch_t_test(&A, &B, Tail::TwoSided, 0.95).expect("welch");
        assert!(r.statistic > 0.0, "treatment mean is higher");
        assert!(r.p_value < 1e-6, "p={} should be tiny", r.p_value);
        // df between min(n-1) and n1+n2-2.
        let df = r.df.expect("df");
        assert!(df > 9.0 && df <= 18.0, "df={df}");
        // Cohen's d is large (well above 0.8).
        assert!(r.effect_size.expect("d") > 2.0);
        // CI on the (positive) difference excludes 0.
        assert!(r.ci_low.expect("lo") > 0.0);
    }

    #[test]
    fn welch_one_sided_directions() {
        let greater = welch_t_test(&A, &B, Tail::Greater, 0.95).expect("g");
        let less = welch_t_test(&A, &B, Tail::Less, 0.95).expect("l");
        // Treatment > control: "greater" is significant, "less" is not.
        assert!(greater.p_value < 1e-6);
        assert!(less.p_value > 0.999);
    }

    #[test]
    fn welch_rejects_short_samples() {
        let one = [1.0];
        assert!(matches!(
            welch_t_test(&one, &B, Tail::TwoSided, 0.95),
            Err(StatsError::TooFewSamples { .. })
        ));
    }

    #[test]
    fn mann_whitney_separates_distributions() {
        let r = mann_whitney_u(&A, &B, Tail::TwoSided).expect("mwu");
        // Near-complete separation with ONE tie (A[2] == B[5] == 5.3): of the
        // 100 control×treatment pairs, 99 favor treatment and 1 ties, so
        // U_treatment = 99.5 (the tie contributes ½; R_control=55.5,
        // R_treatment=154.5, U=154.5−55) and Cliff's δ = 99/100 = 0.99.
        // Asserting these exact values also exercises average-rank tie handling.
        assert!(approx(r.statistic, 99.5, 1e-9));
        assert!(approx(r.effect_size.expect("delta"), 0.99, 1e-9));
        assert!(r.p_value < 1e-3);
    }

    #[test]
    fn mann_whitney_no_difference() {
        let r = mann_whitney_u(&A, &A, Tail::TwoSided).expect("mwu");
        assert!(r.p_value > 0.5, "identical arms → not significant");
        assert!(approx(r.effect_size.expect("delta"), 0.0, 1e-9));
    }

    #[test]
    fn cliffs_delta_signs() {
        assert!(cliffs_delta(&A, &B) > 0.9); // treatment dominates
        assert!(cliffs_delta(&B, &A) < -0.9); // reversed
    }

    #[test]
    fn wilcoxon_paired_shift() {
        // Each B paired with each A; consistent positive shift.
        let r = wilcoxon_signed_rank(&A, &B, Tail::Greater).expect("wsr");
        assert!(r.p_value < 0.01);
        assert!(r.effect_size.expect("rb") > 0.9);
    }

    #[test]
    fn wilcoxon_length_mismatch() {
        let short = [1.0, 2.0, 3.0];
        assert!(matches!(
            wilcoxon_signed_rank(&short, &B, Tail::TwoSided),
            Err(StatsError::LengthMismatch { .. })
        ));
    }

    #[test]
    fn bootstrap_ci_excludes_zero_for_real_difference() {
        let cfg = BootstrapConfig {
            resamples: 2000,
            ci_level: 0.95,
            method: BootMethod::Percentile,
            seed: 42,
        };
        let r = bootstrap_diff_means(&A, &B, &cfg).expect("boot");
        assert!(r.statistic > 0.0);
        assert!(r.ci_low.expect("lo") > 0.0, "CI should exclude 0");
        assert!(r.ci_high.expect("hi") > r.ci_low.expect("lo"));
    }

    #[test]
    fn bootstrap_bca_runs_and_brackets() {
        let cfg = BootstrapConfig {
            resamples: 2000,
            ci_level: 0.95,
            method: BootMethod::Bca,
            seed: 7,
        };
        let r = bootstrap_diff_means(&A, &B, &cfg).expect("bca");
        let (lo, hi) = (r.ci_low.expect("lo"), r.ci_high.expect("hi"));
        assert!(lo <= r.statistic && r.statistic <= hi, "estimate within CI");
        assert!(lo > 0.0);
    }

    #[test]
    fn bootstrap_reproducible_with_seed() {
        let cfg = BootstrapConfig {
            resamples: 1000,
            ci_level: 0.95,
            method: BootMethod::Percentile,
            seed: 123,
        };
        let r1 = bootstrap_diff_means(&A, &B, &cfg).expect("r1");
        let r2 = bootstrap_diff_means(&A, &B, &cfg).expect("r2");
        assert_eq!(r1.ci_low, r2.ci_low);
        assert_eq!(r1.ci_high, r2.ci_high);
    }

    #[test]
    fn tost_equivalent_when_close() {
        // Two samples with near-identical means → equivalent within ±0.5.
        let c = [10.0, 10.1, 9.9, 10.05, 9.95, 10.0, 10.02, 9.98, 10.0, 10.0];
        let t = [
            10.02, 9.97, 10.05, 9.99, 10.01, 10.0, 9.98, 10.03, 9.96, 10.0,
        ];
        let r = tost_equivalence(&c, &t, -0.5, 0.5, 0.05).expect("tost");
        // CI well inside the margin → both one-sided tests reject.
        assert!(r.p_value < 0.05, "TOST p={}", r.p_value);
        assert!(r.ci_low.expect("lo") > -0.5 && r.ci_high.expect("hi") < 0.5);
    }

    #[test]
    fn tost_not_equivalent_when_far() {
        let r = tost_equivalence(&A, &B, -0.1, 0.1, 0.05).expect("tost");
        // ~0.5 difference is outside ±0.1 → not equivalent.
        assert!(r.p_value > 0.05);
    }

    #[test]
    fn anderson_darling_accepts_uniform_spread() {
        // Symmetric, light-tailed sample — should not strongly reject normality.
        let s: Vec<f64> = (0..40).map(|i| i as f64 * 0.1 - 2.0).collect();
        let r = anderson_darling(&s).expect("ad");
        assert!(r.p_value > 0.01);
    }

    #[test]
    fn dagostino_flags_skew() {
        // Strongly right-skewed sample → reject normality.
        let s: Vec<f64> = (0..50).map(|i| ((i * i) as f64) * 0.01).collect();
        let r = dagostino_pearson(&s).expect("dp");
        assert!(!r.is_normalish, "skewed sample should reject normality");
    }

    #[test]
    fn recommend_prefers_nonparametric_when_small() {
        let (rec, notes) = recommend_two_sample_test(&A, &B);
        assert_eq!(rec, RecommendedTest::MannWhitney); // n=10 < 20
        assert!(!notes.is_empty());
    }

    #[test]
    fn benjamini_hochberg_matches_known_vector() {
        // R: p.adjust(c(0.01,0.02,0.03,0.04,0.05), "BH")
        //  = 0.05 0.05 0.05 0.05 0.05
        let p = [0.01, 0.02, 0.03, 0.04, 0.05];
        let adj = adjust_pvalues(&p, Correction::BenjaminiHochberg);
        for v in &adj {
            assert!(approx(*v, 0.05, 1e-9), "got {v}");
        }
    }

    #[test]
    fn bonferroni_scales_by_m() {
        let p = [0.01, 0.5];
        let adj = adjust_pvalues(&p, Correction::Bonferroni);
        assert!(approx(adj[0], 0.02, 1e-12));
        assert!(approx(adj[1], 1.0, 1e-12)); // clamped
    }

    #[test]
    fn power_sample_size_reasonable() {
        // Medium effect d=0.5, alpha=0.05 two-sided, power=0.8 → ~64/arm.
        let n = required_n_per_arm(0.5, 0.05, 0.8, Tail::TwoSided);
        assert!((60..=70).contains(&n), "n={n}");
        // Larger effect needs fewer samples.
        let n_big = required_n_per_arm(1.0, 0.05, 0.8, Tail::TwoSided);
        assert!(n_big < n);
    }

    // ------------------------------------------------------------------------
    // Exact binomial test
    // ------------------------------------------------------------------------

    #[test]
    fn exact_binomial_two_sided_8_of_10() {
        // R: binom.test(8, 10, 0.5)$p.value = 0.109375 (doubling rule).
        let p = exact_binomial_test(8, 10, 0.5, Tail::TwoSided).expect("binom");
        assert!(approx(p, 0.1094, 1e-3), "p={p}");
    }

    #[test]
    fn exact_binomial_two_sided_at_median_is_one() {
        // 5 of 10 under p0=0.5 sits at the median → two-sided p saturates to 1.0.
        let p = exact_binomial_test(5, 10, 0.5, Tail::TwoSided).expect("binom");
        assert!(approx(p, 1.0, 1e-12), "p={p}");
    }

    #[test]
    fn exact_binomial_two_sided_boundary_zero() {
        // 0 of 10 successes under p0=0.5: two-sided p = 2·P(X=0) = 2·(1/1024).
        let p = exact_binomial_test(0, 10, 0.5, Tail::TwoSided).expect("binom");
        assert!(approx(p, 0.00195, 1e-3), "p={p}");
    }

    #[test]
    fn exact_binomial_one_sided_tails_complementary_at_extreme() {
        // At the lower extreme: P(X ≤ 0) is tiny; P(X ≥ 0) is exactly 1.0.
        let left = exact_binomial_test(0, 10, 0.5, Tail::Less).expect("left");
        let right = exact_binomial_test(0, 10, 0.5, Tail::Greater).expect("right");
        assert!(approx(left, 1.0 / 1024.0, 1e-9), "left={left}");
        assert!(approx(right, 1.0, 1e-12), "right={right}");
    }

    #[test]
    fn exact_binomial_validates_inputs() {
        assert!(matches!(
            exact_binomial_test(11, 10, 0.5, Tail::TwoSided),
            Err(StatsError::InvalidParam(_))
        ));
        assert!(matches!(
            exact_binomial_test(5, 10, 0.0, Tail::TwoSided),
            Err(StatsError::InvalidParam(_))
        ));
        assert!(matches!(
            exact_binomial_test(5, 10, 1.0, Tail::TwoSided),
            Err(StatsError::InvalidParam(_))
        ));
    }

    #[test]
    fn exact_binomial_normal_fallback_for_large_n() {
        // n > 1000 uses the normal approximation; a 60%-success vs p0=0.5 over
        // 2000 trials is overwhelmingly significant two-sided.
        let p = exact_binomial_test(1200, 2000, 0.5, Tail::TwoSided).expect("approx");
        assert!(p < 1e-6, "p={p}");
        // And a result right at the null mean is non-significant.
        let p_mid = exact_binomial_test(1000, 2000, 0.5, Tail::TwoSided).expect("approx");
        assert!(p_mid > 0.5, "p_mid={p_mid}");
    }

    // ------------------------------------------------------------------------
    // McNemar's test
    // ------------------------------------------------------------------------

    #[test]
    fn mcnemar_chi2_path() {
        // b=30, c=10, n=40 > 25 → χ² path.
        // statistic = (|30-10| - 1)² / 40 = 19² / 40 = 361/40 = 9.025.
        // χ²(1) upper tail of 9.025 ≈ 0.002663 (SciPy chi2.sf(9.025, 1)).
        let r = mcnemar_test(0, 30, 10, 0).expect("mcnemar");
        assert!(!r.exact, "n=40 should use the χ² path");
        assert!(approx(r.statistic, 9.025, 1e-3), "stat={}", r.statistic);
        assert!(approx(r.p_value, 0.00266, 1e-3), "p={}", r.p_value);
        assert_eq!(r.n_discordant, 40);
        // c - b = 10 - 30 = -20 (treatment broke more than it fixed).
        assert!(approx(r.effect_treatment_minus_control, -20.0, 1e-12));
    }

    #[test]
    fn mcnemar_exact_path() {
        // b=12, c=5, n=17 ≤ 25 → exact path; two-sided exact p ≈ 0.143463.
        let r = mcnemar_test(50, 12, 5, 33).expect("mcnemar");
        assert!(r.exact, "n=17 should use the exact path");
        assert!(approx(r.p_value, 0.1435, 1e-3), "p={}", r.p_value);
        // Exact path reports |b - c| = 7 as the statistic.
        assert!(approx(r.statistic, 7.0, 1e-12), "stat={}", r.statistic);
        assert_eq!(r.n_discordant, 17);
        // c - b = 5 - 12 = -7.
        assert!(approx(r.effect_treatment_minus_control, -7.0, 1e-12));
    }

    #[test]
    fn mcnemar_symmetric_discordant_is_one() {
        // b = c = 8 → perfectly symmetric disagreement → p = 1.0.
        let r = mcnemar_test(20, 8, 8, 4).expect("mcnemar");
        assert!(r.exact, "n=16 should use the exact path");
        assert!(approx(r.p_value, 1.0, 1e-12), "p={}", r.p_value);
        assert!(approx(r.statistic, 0.0, 1e-12), "stat={}", r.statistic);
        assert!(approx(r.effect_treatment_minus_control, 0.0, 1e-12));
    }

    #[test]
    fn mcnemar_no_discordant_pairs() {
        // n = 0 → no evidence of a difference.
        let r = mcnemar_test(40, 0, 0, 10).expect("mcnemar");
        assert_eq!(r.n_discordant, 0);
        assert!(approx(r.p_value, 1.0, 1e-12), "p={}", r.p_value);
        assert!(approx(r.statistic, 0.0, 1e-12), "stat={}", r.statistic);
        assert!(r.exact);
        assert!(approx(r.effect_treatment_minus_control, 0.0, 1e-12));
    }

    #[test]
    fn mcnemar_symmetry_under_arm_swap() {
        // Swapping b and c must leave the p-value unchanged and negate the
        // effect (the test is on the magnitude of disagreement; the direction
        // is carried by the sign of c - b). Exercised on both paths.
        for (b, c) in [(12_u64, 5_u64), (30, 10)] {
            let r = mcnemar_test(7, b, c, 9).expect("orig");
            let swapped = mcnemar_test(7, c, b, 9).expect("swapped");
            assert!(
                approx(r.p_value, swapped.p_value, 1e-12),
                "p mismatch for (b={b}, c={c}): {} vs {}",
                r.p_value,
                swapped.p_value
            );
            assert!(approx(r.statistic, swapped.statistic, 1e-12));
            assert!(approx(
                r.effect_treatment_minus_control,
                -swapped.effect_treatment_minus_control,
                1e-12
            ));
            assert_eq!(r.n_discordant, swapped.n_discordant);
            assert_eq!(r.exact, swapped.exact);
        }
    }
}
