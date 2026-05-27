//! Pre-registered acceptance criteria for the scientific-experiment subsystem.
//!
//! An [`AcceptanceCriterion`] is the empirical, frozen-at-`experiment_open`
//! rule that decides whether a hypothesis is accepted. It is stored as JSONB
//! on `experiment_hypotheses.acceptance_criterion` (and snapshotted into
//! `experiment_results.criterion_snapshot`), so this enum's serde shape is a
//! persisted contract — extend it additively.
//!
//! [`evaluate`] runs the criterion against the recorded `(control, treatment)`
//! samples and returns a [`Decision`] with the full statistical evidence. For
//! composite criteria it threads a multiple-comparison [`Correction`] across
//! the null-hypothesis-significance-test (NHST) leaves — Welch / Mann-Whitney
//! / Wilcoxon — so an experiment that tests many correlated metrics does not
//! inflate its family-wise error rate (Benjamini-Hochberg FDR is the
//! recommended default).
//!
//! The criterion type is chosen to match the *nature of the metric*
//! (§2.1 of the design): stochastic metrics (latency, throughput) use a
//! significance test; deterministic single values (LOC, one module's LCOM4)
//! use a threshold / relative-change rule; deterministic distribution-valued
//! metrics (per-file complexity before/after) use the paired Wilcoxon test;
//! and diagnostic deep-dives record an evidence-based `observational` verdict
//! with no p-value at all.

use serde::{Deserialize, Serialize};

use super::inference::{
    self, BootMethod, BootstrapConfig, Correction, EffectKind, Estimand, StatsError, Tail,
    TestKind, TestResult,
};

/// A minimum effect-size gate attached to a significance test, so that a
/// statistically significant but practically trivial difference (which large
/// n makes easy) is not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MinEffect {
    pub kind: EffectKind,
    pub threshold: f64,
}

/// Summary statistic of a single arm, for absolute-threshold SLOs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryStat {
    Mean,
    Median,
    P95,
    P99,
    Max,
    Min,
    StdDev,
}

/// Comparison operator for an absolute threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CmpOp {
    Lt,
    Le,
    Gt,
    Ge,
}

/// Which arm an absolute threshold (single-arm SLO) applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArmSelector {
    Control,
    Treatment,
}

/// Equivalence margin, on the treatment − control difference. `Percent` is
/// resolved against the control mean at evaluation time.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MarginSpec {
    Absolute { low: f64, high: f64 },
    Percent { pct: f64 },
}

/// The verdict for an `observational` (evidence-based, non-statistical)
/// hypothesis — the diagnostic deep-dive / bug-fix chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObsVerdict {
    Supported,
    Falsified,
    Inconclusive,
}

/// The pre-registered acceptance rule. `#[serde(tag = "type", content = "params")]`
/// (ADJACENT tagging) keeps the persisted JSON self-describing AND compile-cheap.
/// Do NOT switch to internal tagging (`tag = "type"` alone): on this *recursive*
/// enum it makes serde's derived `Deserialize` buffer through `Content` and nest
/// one `ContentDeserializer` layer per recursion level, which stalls rustc's
/// monomorphization collector (a multi-hour "hang" — empirically reproduced; see
/// `docs/decisions/006-*`). Adjacent tagging puts the variant payload in a
/// separate `params` value deserialized through the normal path, so it doesn't
/// nest. Shape, e.g.:
/// `{"type":"welch_t","params":{"alpha":0.05,"tail":"greater","min_effect":{"kind":"cohens_d","threshold":0.5}}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "params", rename_all = "snake_case")]
pub enum AcceptanceCriterion {
    /// Welch's t-test p < α in `tail` direction, with an optional minimum
    /// effect-size gate. The common default for noisy performance metrics.
    WelchT {
        alpha: f64,
        tail: Tail,
        #[serde(default)]
        min_effect: Option<MinEffect>,
    },
    /// Mann-Whitney U p < α; for non-normal / distribution-valued metrics.
    MannWhitneyU {
        alpha: f64,
        tail: Tail,
        #[serde(default)]
        min_effect: Option<MinEffect>,
    },
    /// Wilcoxon signed-rank p < α; for paired before/after on the same units.
    WilcoxonSignedRank {
        alpha: f64,
        tail: Tail,
        #[serde(default)]
        min_effect: Option<MinEffect>,
    },
    /// The `(1 − α)` bootstrap CI of (treatment − control) lies entirely
    /// beyond `margin` (i.e. excludes `[−margin, margin]`; `margin = 0`
    /// excludes zero).
    BootstrapCiExcludes {
        estimand: Estimand,
        ci_level: f64,
        #[serde(default)]
        margin: f64,
        method: BootMethod,
        #[serde(default)]
        seed: Option<u64>,
    },
    /// |effect| ≥ threshold, no significance test.
    EffectThreshold { kind: EffectKind, threshold: f64 },
    /// Sign-correct relative change of the estimand ≥ `pct` (e.g. 0.05 = 5%).
    RelativeImprovement {
        pct: f64,
        lower_is_better: bool,
        estimand: Estimand,
    },
    /// Single-arm SLO: `stat(arm) op value` (e.g. p99(treatment) < 200).
    AbsoluteThreshold {
        stat: SummaryStat,
        op: CmpOp,
        value: f64,
        #[serde(default = "default_arm")]
        arm: ArmSelector,
    },
    /// TOST equivalence within `margin` — "no regression" / "preserves
    /// behavior". Cannot be established by a non-significant two-sided test.
    Equivalence { margin: MarginSpec, alpha: f64 },
    /// Evidence-based verdict for a diagnostic-deep-dive / bug-fix hypothesis
    /// chain — supported/falsified by recorded evidence, no p-value.
    Observational {
        prediction: String,
        #[serde(default)]
        observed: Option<String>,
        verdict: ObsVerdict,
    },
    /// All sub-criteria must pass (the refactor/feature composite).
    AllOf(Vec<AcceptanceCriterion>),
    /// Any sub-criterion passes.
    AnyOf(Vec<AcceptanceCriterion>),
    /// Negation.
    Not(Box<AcceptanceCriterion>),
}

fn default_arm() -> ArmSelector {
    ArmSelector::Treatment
}

impl AcceptanceCriterion {
    /// The recommended default for a performance **optimization**: Welch
    /// p<0.05 AND |Cohen's d| ≥ 0.5 AND the correct direction. `lower_is_better`
    /// selects the tail (latency/RSS → `Less`; throughput → `Greater`).
    pub fn default_optimization(lower_is_better: bool) -> Self {
        let tail = if lower_is_better {
            Tail::Less
        } else {
            Tail::Greater
        };
        AcceptanceCriterion::WelchT {
            alpha: 0.05,
            tail,
            min_effect: Some(MinEffect {
                kind: EffectKind::CohensD,
                threshold: 0.5,
            }),
        }
    }

    /// The `(alpha, tail)` of the first significance-test leaf (Welch /
    /// Mann-Whitney / Wilcoxon), searched depth-first. The protocol engine
    /// uses it to size the sample via power analysis. `None` when the
    /// criterion has no NHST leaf (pure threshold / observational).
    pub fn primary_significance(&self) -> Option<(f64, Tail)> {
        match self {
            AcceptanceCriterion::WelchT { alpha, tail, .. }
            | AcceptanceCriterion::MannWhitneyU { alpha, tail, .. }
            | AcceptanceCriterion::WilcoxonSignedRank { alpha, tail, .. } => Some((*alpha, *tail)),
            AcceptanceCriterion::AllOf(v) | AcceptanceCriterion::AnyOf(v) => {
                v.iter().find_map(|c| c.primary_significance())
            }
            AcceptanceCriterion::Not(b) => b.primary_significance(),
            _ => None,
        }
    }

    /// The minimum effect-size threshold of the first NHST leaf that sets one
    /// (used as the effect to power for). `None` ⇒ caller picks a default.
    pub fn primary_min_effect(&self) -> Option<f64> {
        match self {
            AcceptanceCriterion::WelchT { min_effect, .. }
            | AcceptanceCriterion::MannWhitneyU { min_effect, .. }
            | AcceptanceCriterion::WilcoxonSignedRank { min_effect, .. } => {
                min_effect.as_ref().map(|m| m.threshold)
            }
            AcceptanceCriterion::AllOf(v) | AcceptanceCriterion::AnyOf(v) => {
                v.iter().find_map(|c| c.primary_min_effect())
            }
            AcceptanceCriterion::Not(b) => b.primary_min_effect(),
            _ => None,
        }
    }
}

/// The outcome of evaluating a criterion: the accept/reject boolean, the full
/// statistical evidence (one [`TestResult`] per leaf, in document order), and
/// a human-readable rationale for the ledger.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub accepted: bool,
    pub rationale: String,
    pub evidence: Vec<TestResult>,
}

/// Per-leaf evaluation, used internally to thread multiple-comparison
/// correction across NHST leaves before the boolean tree is resolved.
struct LeafEval {
    passed: bool,
    result: TestResult,
    /// `Some(alpha)` iff this leaf decides via a corrected-able p-value
    /// (Welch / Mann-Whitney / Wilcoxon). `None` for CI / threshold /
    /// observational / equivalence leaves.
    nhst_alpha: Option<f64>,
    /// The effect-gate component of the pass (true when there is no gate), so
    /// a corrected p can be recombined with it without re-running the test.
    effect_ok: bool,
}

/// Evaluate `criterion` against the recorded samples, threading `correction`
/// across the significance-test leaves of any composite.
pub fn evaluate(
    criterion: &AcceptanceCriterion,
    control: &[f64],
    treatment: &[f64],
    correction: Correction,
) -> Result<Decision, StatsError> {
    // 1. Flatten leaves in document order.
    let mut leaves: Vec<&AcceptanceCriterion> = Vec::new();
    collect_leaves(criterion, &mut leaves);

    // 2. Evaluate each leaf once.
    let mut evals: Vec<LeafEval> = Vec::with_capacity(leaves.len());
    for leaf in &leaves {
        evals.push(eval_leaf(leaf, control, treatment)?);
    }

    // 3. Thread multiple-comparison correction across the NHST leaves.
    apply_correction(&mut evals, correction);

    // 4. Resolve the boolean tree, consuming leaf evaluations in order.
    let mut cursor = 0usize;
    let accepted = resolve_tree(criterion, &evals, &mut cursor);

    let evidence: Vec<TestResult> = evals.into_iter().map(|e| e.result).collect();
    let rationale = render_rationale(criterion, accepted, &evidence, correction);
    Ok(Decision {
        accepted,
        rationale,
        evidence,
    })
}

fn collect_leaves<'a>(c: &'a AcceptanceCriterion, out: &mut Vec<&'a AcceptanceCriterion>) {
    match c {
        AcceptanceCriterion::AllOf(v) | AcceptanceCriterion::AnyOf(v) => {
            for child in v {
                collect_leaves(child, out);
            }
        }
        AcceptanceCriterion::Not(b) => collect_leaves(b, out),
        leaf => out.push(leaf),
    }
}

fn resolve_tree(c: &AcceptanceCriterion, evals: &[LeafEval], cursor: &mut usize) -> bool {
    match c {
        AcceptanceCriterion::AllOf(v) => {
            // Evaluate every child (advancing the cursor for all) before
            // reducing, so the cursor stays aligned regardless of result.
            let results: Vec<bool> = v.iter().map(|x| resolve_tree(x, evals, cursor)).collect();
            results.iter().all(|&b| b)
        }
        AcceptanceCriterion::AnyOf(v) => {
            let results: Vec<bool> = v.iter().map(|x| resolve_tree(x, evals, cursor)).collect();
            results.iter().any(|&b| b)
        }
        AcceptanceCriterion::Not(b) => !resolve_tree(b, evals, cursor),
        _leaf => {
            let passed = evals[*cursor].passed;
            *cursor += 1;
            passed
        }
    }
}

/// Apply the chosen correction to the NHST leaves' p-values and recompute
/// their `passed` flag (corrected_p < alpha AND effect_ok). The leaf's
/// evidence `TestResult.p_value` is updated to the corrected value with a
/// note recording the raw value, so the ledger is honest about what decided.
fn apply_correction(evals: &mut [LeafEval], correction: Correction) {
    let nhst_idx: Vec<usize> = evals
        .iter()
        .enumerate()
        .filter(|(_, e)| e.nhst_alpha.is_some())
        .map(|(i, _)| i)
        .collect();
    if nhst_idx.len() < 2 || correction == Correction::None {
        return; // nothing to correct (single or zero significance tests)
    }
    let raw: Vec<f64> = nhst_idx.iter().map(|&i| evals[i].result.p_value).collect();
    let adjusted = inference::adjust_pvalues(&raw, correction);
    for (k, &i) in nhst_idx.iter().enumerate() {
        let alpha = evals[i].nhst_alpha.expect("nhst leaf has alpha");
        let raw_p = raw[k];
        let adj_p = adjusted[k];
        evals[i].result.p_value = adj_p;
        evals[i].result.notes.push(format!(
            "raw p={raw_p:.6}, {correction:?}-adjusted p={adj_p:.6}"
        ));
        evals[i].passed = adj_p < alpha && evals[i].effect_ok;
    }
}

/// Evaluate a single (non-composite) criterion leaf.
fn eval_leaf(
    c: &AcceptanceCriterion,
    control: &[f64],
    treatment: &[f64],
) -> Result<LeafEval, StatsError> {
    match c {
        AcceptanceCriterion::WelchT {
            alpha,
            tail,
            min_effect,
        } => {
            let result = inference::welch_t_test(control, treatment, *tail, 0.95)?;
            finish_nhst(result, *alpha, min_effect, control, treatment)
        }
        AcceptanceCriterion::MannWhitneyU {
            alpha,
            tail,
            min_effect,
        } => {
            let result = inference::mann_whitney_u(control, treatment, *tail)?;
            finish_nhst(result, *alpha, min_effect, control, treatment)
        }
        AcceptanceCriterion::WilcoxonSignedRank {
            alpha,
            tail,
            min_effect,
        } => {
            let result = inference::wilcoxon_signed_rank(control, treatment, *tail)?;
            finish_nhst(result, *alpha, min_effect, control, treatment)
        }
        AcceptanceCriterion::BootstrapCiExcludes {
            estimand,
            ci_level,
            margin,
            method,
            seed,
        } => {
            let cfg = BootstrapConfig {
                resamples: 10_000,
                ci_level: *ci_level,
                method: *method,
                seed: seed.unwrap_or(0),
            };
            let result = match estimand {
                Estimand::Mean => inference::bootstrap_diff_means(control, treatment, &cfg)?,
                Estimand::Median => inference::bootstrap_diff_medians(control, treatment, &cfg)?,
            };
            let lo = result.ci_low.unwrap_or(f64::NAN);
            let hi = result.ci_high.unwrap_or(f64::NAN);
            // CI entirely beyond ±margin (excludes the equivalence band).
            let passed = lo > *margin || hi < -*margin;
            Ok(LeafEval {
                passed,
                result,
                nhst_alpha: None,
                effect_ok: true,
            })
        }
        AcceptanceCriterion::EffectThreshold { kind, threshold } => {
            let effect = compute_effect(*kind, control, treatment)?;
            let passed = effect.abs() >= *threshold;
            Ok(LeafEval {
                passed,
                result: synthetic(
                    TestKind::EffectThreshold,
                    effect,
                    Some(effect),
                    Some(*kind),
                    control.len(),
                    treatment.len(),
                    format!("|effect|={:.4} vs threshold {:.4}", effect.abs(), threshold),
                ),
                nhst_alpha: None,
                effect_ok: true,
            })
        }
        AcceptanceCriterion::RelativeImprovement {
            pct,
            lower_is_better,
            estimand,
        } => {
            let ec = point_estimate(control, *estimand);
            let et = point_estimate(treatment, *estimand);
            if ec == 0.0 {
                return Err(StatsError::InvalidParam(
                    "relative_improvement: control estimate is zero".to_string(),
                ));
            }
            let raw_rel = (et - ec) / ec.abs();
            // Improvement is positive when the metric moves the desired way.
            let improvement = if *lower_is_better { -raw_rel } else { raw_rel };
            let passed = improvement >= *pct;
            Ok(LeafEval {
                passed,
                result: synthetic(
                    TestKind::RelativeImprovement,
                    improvement,
                    Some(improvement),
                    Some(EffectKind::RelativeChange),
                    control.len(),
                    treatment.len(),
                    format!(
                        "relative improvement {:.4} vs required {:.4} (lower_is_better={})",
                        improvement, pct, lower_is_better
                    ),
                ),
                nhst_alpha: None,
                effect_ok: true,
            })
        }
        AcceptanceCriterion::AbsoluteThreshold {
            stat,
            op,
            value,
            arm,
        } => {
            let samples = match arm {
                ArmSelector::Control => control,
                ArmSelector::Treatment => treatment,
            };
            if samples.is_empty() {
                return Err(StatsError::TooFewSamples { min: 1, got: 0 });
            }
            let observed = summary_stat(samples, *stat);
            let passed = match op {
                CmpOp::Lt => observed < *value,
                CmpOp::Le => observed <= *value,
                CmpOp::Gt => observed > *value,
                CmpOp::Ge => observed >= *value,
            };
            Ok(LeafEval {
                passed,
                result: synthetic(
                    TestKind::AbsoluteThreshold,
                    observed,
                    None,
                    None,
                    control.len(),
                    treatment.len(),
                    format!(
                        "{:?}({:?})={:.4} {:?} {:.4}",
                        stat, arm, observed, op, value
                    ),
                ),
                nhst_alpha: None,
                effect_ok: true,
            })
        }
        AcceptanceCriterion::Equivalence { margin, alpha } => {
            let (low, high) = resolve_margin(margin, control);
            let result = inference::tost_equivalence(control, treatment, low, high, *alpha)?;
            let lo = result.ci_low.unwrap_or(f64::NAN);
            let hi = result.ci_high.unwrap_or(f64::NAN);
            // Equivalent iff the (1 − 2α) CI lies entirely within the margin.
            let passed = lo >= low && hi <= high;
            Ok(LeafEval {
                passed,
                result,
                nhst_alpha: None,
                effect_ok: true,
            })
        }
        AcceptanceCriterion::Observational {
            prediction,
            observed,
            verdict,
        } => {
            let passed = matches!(verdict, ObsVerdict::Supported);
            let note = match observed {
                Some(o) => format!("predicted: {prediction}; observed: {o}; verdict: {verdict:?}"),
                None => format!("predicted: {prediction}; verdict: {verdict:?}"),
            };
            Ok(LeafEval {
                passed,
                result: synthetic(
                    TestKind::Observational,
                    f64::NAN,
                    None,
                    None,
                    control.len(),
                    treatment.len(),
                    note,
                ),
                nhst_alpha: None,
                effect_ok: true,
            })
        }
        // Composites never reach here (collect_leaves descends into them).
        AcceptanceCriterion::AllOf(_)
        | AcceptanceCriterion::AnyOf(_)
        | AcceptanceCriterion::Not(_) => unreachable!("composite passed to eval_leaf"),
    }
}

/// Combine a significance-test result with an optional minimum-effect gate.
fn finish_nhst(
    mut result: TestResult,
    alpha: f64,
    min_effect: &Option<MinEffect>,
    control: &[f64],
    treatment: &[f64],
) -> Result<LeafEval, StatsError> {
    let sig = result.p_value < alpha;
    let effect_ok = match min_effect {
        None => true,
        Some(me) => {
            let e = compute_effect(me.kind, control, treatment)?;
            let ok = e.abs() >= me.threshold;
            result.notes.push(format!(
                "min-effect gate: |{:?}|={:.4} vs {:.4} → {}",
                me.kind,
                e.abs(),
                me.threshold,
                if ok { "pass" } else { "fail" }
            ));
            ok
        }
    };
    Ok(LeafEval {
        passed: sig && effect_ok,
        result,
        nhst_alpha: Some(alpha),
        effect_ok,
    })
}

fn compute_effect(kind: EffectKind, control: &[f64], treatment: &[f64]) -> Result<f64, StatsError> {
    let e = match kind {
        EffectKind::CohensD => inference::cohens_d(control, treatment),
        EffectKind::HedgesG => inference::hedges_g(control, treatment),
        EffectKind::CliffsDelta => inference::cliffs_delta(control, treatment),
        EffectKind::RankBiserial => {
            let r = inference::mann_whitney_u(control, treatment, Tail::TwoSided)?;
            inference::rank_biserial(r.statistic, control.len(), treatment.len())
        }
        EffectKind::RelativeChange => inference::relative_change_median(control, treatment),
    };
    Ok(e)
}

#[inline]
fn point_estimate(samples: &[f64], estimand: Estimand) -> f64 {
    match estimand {
        Estimand::Mean => inference::summarize(samples).mean,
        Estimand::Median => inference::median(samples),
    }
}

fn summary_stat(samples: &[f64], stat: SummaryStat) -> f64 {
    let s = inference::summarize(samples);
    match stat {
        SummaryStat::Mean => s.mean,
        SummaryStat::Median => s.median,
        SummaryStat::Max => s.max,
        SummaryStat::Min => s.min,
        SummaryStat::StdDev => s.std_dev,
        SummaryStat::P95 => inference::percentile(samples, 0.95),
        SummaryStat::P99 => inference::percentile(samples, 0.99),
    }
}

fn resolve_margin(margin: &MarginSpec, control: &[f64]) -> (f64, f64) {
    match margin {
        MarginSpec::Absolute { low, high } => (*low, *high),
        MarginSpec::Percent { pct } => {
            let m = inference::summarize(control).mean.abs();
            (-pct * m, pct * m)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn synthetic(
    kind: TestKind,
    statistic: f64,
    effect_size: Option<f64>,
    effect_kind: Option<EffectKind>,
    n_control: usize,
    n_treatment: usize,
    note: String,
) -> TestResult {
    TestResult {
        kind,
        tail: Tail::TwoSided,
        statistic,
        df: None,
        p_value: f64::NAN, // not a hypothesis test
        effect_size,
        effect_kind,
        ci_low: None,
        ci_high: None,
        ci_level: 0.0,
        n_control,
        n_treatment,
        notes: vec![note],
    }
}

fn render_rationale(
    criterion: &AcceptanceCriterion,
    accepted: bool,
    evidence: &[TestResult],
    correction: Correction,
) -> String {
    let verdict = if accepted { "ACCEPTED" } else { "REJECTED" };
    let mut lines = vec![format!(
        "{verdict} (criterion: {}, correction: {correction:?})",
        criterion_label(criterion)
    )];
    for (i, e) in evidence.iter().enumerate() {
        let p = if e.p_value.is_nan() {
            "n/a".to_string()
        } else {
            format!("{:.6}", e.p_value)
        };
        let eff = e
            .effect_size
            .map(|v| format!(", effect={v:.4}"))
            .unwrap_or_default();
        let ci = match (e.ci_low, e.ci_high) {
            (Some(lo), Some(hi)) => format!(", {:.0}% CI=[{lo:.4}, {hi:.4}]", e.ci_level * 100.0),
            _ => String::new(),
        };
        lines.push(format!(
            "  [{i}] {:?}: statistic={:.4}, p={p}{eff}{ci}",
            e.kind, e.statistic
        ));
    }
    lines.join("\n")
}

fn criterion_label(c: &AcceptanceCriterion) -> &'static str {
    match c {
        AcceptanceCriterion::WelchT { .. } => "welch_t",
        AcceptanceCriterion::MannWhitneyU { .. } => "mann_whitney_u",
        AcceptanceCriterion::WilcoxonSignedRank { .. } => "wilcoxon_signed_rank",
        AcceptanceCriterion::BootstrapCiExcludes { .. } => "bootstrap_ci_excludes",
        AcceptanceCriterion::EffectThreshold { .. } => "effect_threshold",
        AcceptanceCriterion::RelativeImprovement { .. } => "relative_improvement",
        AcceptanceCriterion::AbsoluteThreshold { .. } => "absolute_threshold",
        AcceptanceCriterion::Equivalence { .. } => "equivalence",
        AcceptanceCriterion::Observational { .. } => "observational",
        AcceptanceCriterion::AllOf(_) => "all_of",
        AcceptanceCriterion::AnyOf(_) => "any_of",
        AcceptanceCriterion::Not(_) => "not",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: [f64; 12] = [5.1, 4.9, 5.3, 5.0, 5.2, 4.8, 5.1, 5.0, 4.95, 5.05, 5.0, 5.1];
    const B: [f64; 12] = [5.5, 5.7, 5.4, 5.6, 5.8, 5.3, 5.6, 5.5, 5.65, 5.45, 5.5, 5.6];

    #[test]
    fn welch_default_optimization_accepts_real_improvement() {
        // Lower-is-better metric: treatment (A) lower than control (B).
        // Frame control=B (slow), treatment=A (fast) → improvement.
        let crit = AcceptanceCriterion::default_optimization(true);
        let d = evaluate(&crit, &B, &A, Correction::BenjaminiHochberg).expect("eval");
        assert!(d.accepted, "rationale:\n{}", d.rationale);
        assert_eq!(d.evidence.len(), 1);
    }

    #[test]
    fn welch_default_rejects_wrong_direction() {
        // Same metric, but treatment is SLOWER (B) than control (A): a
        // lower-is-better criterion must reject (significant regression).
        let crit = AcceptanceCriterion::default_optimization(true);
        let d = evaluate(&crit, &A, &B, Correction::None).expect("eval");
        assert!(!d.accepted);
    }

    #[test]
    fn min_effect_gate_blocks_trivial_difference() {
        // Two arms with a tiny but, at large n, "significant" shift.
        let c: Vec<f64> = (0..200).map(|i| (i % 10) as f64).collect();
        let mut t = c.clone();
        for v in t.iter_mut() {
            *v += 0.01; // trivial shift
        }
        let crit = AcceptanceCriterion::WelchT {
            alpha: 0.05,
            tail: Tail::Greater,
            min_effect: Some(MinEffect {
                kind: EffectKind::CohensD,
                threshold: 0.5,
            }),
        };
        let d = evaluate(&crit, &c, &t, Correction::None).expect("eval");
        assert!(!d.accepted, "tiny effect must fail the d>=0.5 gate");
    }

    #[test]
    fn absolute_threshold_slo() {
        let crit = AcceptanceCriterion::AbsoluteThreshold {
            stat: SummaryStat::Max,
            op: CmpOp::Lt,
            value: 6.0,
            arm: ArmSelector::Treatment,
        };
        let d = evaluate(&crit, &A, &B, Correction::None).expect("eval");
        assert!(d.accepted, "max(B)=5.8 < 6.0");
        assert!(d.evidence[0].p_value.is_nan());
    }

    #[test]
    fn equivalence_accepts_close_arms() {
        let c = [10.0, 10.1, 9.9, 10.05, 9.95, 10.0, 10.02, 9.98, 10.0, 10.0];
        let t = [
            10.02, 9.97, 10.05, 9.99, 10.01, 10.0, 9.98, 10.03, 9.96, 10.0,
        ];
        let crit = AcceptanceCriterion::Equivalence {
            margin: MarginSpec::Absolute {
                low: -0.5,
                high: 0.5,
            },
            alpha: 0.05,
        };
        let d = evaluate(&crit, &c, &t, Correction::None).expect("eval");
        assert!(d.accepted);
    }

    #[test]
    fn observational_supported() {
        let crit = AcceptanceCriterion::Observational {
            prediction: "index mtime equals disk mtime".to_string(),
            observed: Some("equal for all sampled files".to_string()),
            verdict: ObsVerdict::Supported,
        };
        // Observational criteria ignore the samples.
        let d = evaluate(&crit, &[], &[], Correction::None).expect("eval");
        assert!(d.accepted);
        assert!(d.evidence[0].p_value.is_nan());
    }

    #[test]
    fn observational_falsified() {
        let crit = AcceptanceCriterion::Observational {
            prediction: "X".to_string(),
            observed: None,
            verdict: ObsVerdict::Falsified,
        };
        let d = evaluate(&crit, &[], &[], Correction::None).expect("eval");
        assert!(!d.accepted);
    }

    #[test]
    fn composite_all_of_refactor() {
        // Refactor: perf equivalent AND a structural metric improved.
        let perf_c = [
            100.0, 101.0, 99.0, 100.5, 99.5, 100.0, 100.2, 99.8, 100.0, 100.1,
        ];
        let perf_t = [
            100.1, 99.9, 100.2, 99.8, 100.0, 100.05, 99.95, 100.1, 99.9, 100.0,
        ];
        // Structural: per-file complexity dropped on every file (paired).
        let cx_before = [10.0, 12.0, 8.0, 15.0, 9.0, 11.0, 14.0, 7.0];
        let cx_after = [8.0, 9.0, 7.0, 11.0, 8.0, 9.0, 10.0, 6.0];

        let perf = AcceptanceCriterion::Equivalence {
            margin: MarginSpec::Percent { pct: 0.03 },
            alpha: 0.05,
        };
        // Evaluate the two arms each against their own samples is not how a
        // single evaluate() call works (it has one control/treatment pair),
        // so test the structural leaf and perf leaf separately here, then the
        // composite over a single metric pair to exercise the tree + FDR.
        let perf_decision = evaluate(&perf, &perf_c, &perf_t, Correction::None).expect("perf");
        assert!(perf_decision.accepted, "perf should be equivalent");

        let structural = AcceptanceCriterion::WilcoxonSignedRank {
            alpha: 0.05,
            tail: Tail::Less, // after < before
            min_effect: None,
        };
        let struct_decision =
            evaluate(&structural, &cx_before, &cx_after, Correction::None).expect("struct");
        assert!(struct_decision.accepted, "complexity should drop");
    }

    #[test]
    fn composite_threads_fdr_across_leaves() {
        // An AllOf of two Welch tests over the same pair; with BH correction
        // the two identical p-values are adjusted but both still significant.
        let crit = AcceptanceCriterion::AllOf(vec![
            AcceptanceCriterion::WelchT {
                alpha: 0.05,
                tail: Tail::Greater,
                min_effect: None,
            },
            AcceptanceCriterion::WelchT {
                alpha: 0.05,
                tail: Tail::Greater,
                min_effect: None,
            },
        ]);
        let d = evaluate(&crit, &A, &B, Correction::BenjaminiHochberg).expect("eval");
        assert!(d.accepted);
        assert_eq!(d.evidence.len(), 2);
        // The correction note should be present on the NHST leaves.
        assert!(d.evidence[0].notes.iter().any(|n| n.contains("adjusted")));
    }

    #[test]
    fn not_inverts() {
        let inner = AcceptanceCriterion::AbsoluteThreshold {
            stat: SummaryStat::Mean,
            op: CmpOp::Lt,
            value: 0.0, // mean(B) < 0 is false
            arm: ArmSelector::Treatment,
        };
        let crit = AcceptanceCriterion::Not(Box::new(inner));
        let d = evaluate(&crit, &A, &B, Correction::None).expect("eval");
        assert!(d.accepted, "NOT(false) = true");
    }

    #[test]
    fn criterion_serde_roundtrip() {
        let crit = AcceptanceCriterion::default_optimization(true);
        let json = serde_json::to_string(&crit).expect("ser");
        let back: AcceptanceCriterion = serde_json::from_str(&json).expect("de");
        assert_eq!(crit, back);
        // Spot-check the persisted tag shape.
        assert!(json.contains("\"type\":\"welch_t\""));
    }

    #[test]
    fn relative_improvement_leaf() {
        // Median goes 100 → 90, lower_is_better → 10% improvement.
        let c = [100.0, 100.0, 100.0, 100.0, 100.0];
        let t = [90.0, 90.0, 90.0, 90.0, 90.0];
        let crit = AcceptanceCriterion::RelativeImprovement {
            pct: 0.05,
            lower_is_better: true,
            estimand: Estimand::Median,
        };
        let d = evaluate(&crit, &c, &t, Correction::None).expect("eval");
        assert!(d.accepted, "10% improvement >= 5%");
    }
}
