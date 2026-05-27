//! Kind-aware experiment-protocol prescription.
//!
//! The MCP server is the methodology authority. Given an experiment `kind`,
//! the primary metric, and the pre-registered [`AcceptanceCriterion`], it
//! prescribes *how* the agent must run the experiment — the sample size
//! (power analysis), warm-up, the recommended statistical test, the arms, and
//! a reproducibility checklist drawn from the project's benchmarking mandates.
//! The agent executes; the server validates conformance (in
//! `experiment_record_measurement`) and renders the verdict (in
//! `experiment_decide`).
//!
//! This is advisory guidance + hard requirements bundled into one
//! serializable [`Protocol`] returned by `experiment_open` / `experiment_protocol`.

use serde::{Deserialize, Serialize};

use crate::config::ExperimentsConfig;
use crate::stats::acceptance::AcceptanceCriterion;
use crate::stats::inference::{Tail, required_n_per_arm};

/// One arm the agent must collect data for.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArmSpec {
    pub label: String,
    /// `control | treatment | baseline`.
    pub kind: String,
    pub description: String,
}

/// The prescribed experiment design returned to the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Protocol {
    pub kind: String,
    pub primary_metric: String,
    pub unit: Option<String>,
    pub predicted_direction: String,
    /// `stochastic | deterministic_distribution | deterministic_single | observational`.
    pub metric_nature: String,
    /// The statistical test the verdict will use (the agent should collect
    /// data suitable for it).
    pub recommended_test: String,
    /// The frozen, pre-registered acceptance criterion (echoed as JSON).
    pub acceptance_criterion: serde_json::Value,
    /// Minimum replicates per arm for stochastic metrics (power-sized, floored
    /// at `[experiments] min_samples_per_arm`). `None` for non-stochastic
    /// metrics (a single deterministic value, a paired distribution, or
    /// evidence-based observation).
    pub required_samples_per_arm: Option<u32>,
    /// Warm-up replicates to discard before measuring (steady state).
    pub warmup_runs: u32,
    pub arms: Vec<ArmSpec>,
    /// Multiple-comparison correction applied across NHST leaves of a composite.
    pub correction: String,
    /// The data schema the agent must submit to `experiment_record_measurement`.
    pub data_schema: String,
    /// Operational reproducibility requirements (CPU pinning, governor,
    /// hardware capture, exact commands, seed, tee-and-run-once).
    pub reproducibility_checklist: Vec<String>,
    pub notes: Vec<String>,
}

/// Prescribe the protocol for an experiment.
pub fn prescribe(
    kind: &str,
    primary_metric: &str,
    unit: Option<&str>,
    predicted_direction: &str,
    criterion: &AcceptanceCriterion,
    cfg: &ExperimentsConfig,
    expected_effect: Option<f64>,
) -> Protocol {
    let acceptance_criterion = serde_json::to_value(criterion).unwrap_or(serde_json::Value::Null);
    let metric_nature = metric_nature_of(criterion).to_string();
    let recommended_test = recommended_test_of(criterion, &cfg.default_test);

    let required_samples_per_arm = if metric_nature == "stochastic" {
        let (alpha, tail) = criterion
            .primary_significance()
            .unwrap_or((cfg.default_alpha, Tail::TwoSided));
        let effect = expected_effect
            .or_else(|| criterion.primary_min_effect())
            .unwrap_or(0.5);
        let n = required_n_per_arm(effect, alpha, cfg.default_power, tail);
        // `required_n_per_arm` returns usize::MAX for a zero effect; clamp.
        let n = u32::try_from(n).unwrap_or(u32::MAX);
        Some(n.max(cfg.min_samples_per_arm))
    } else {
        None
    };

    let warmup_runs = if metric_nature == "stochastic" { 3 } else { 0 };
    let arms = arms_of(kind);

    let mut notes = Vec::new();
    notes.push(match metric_nature.as_str() {
        "stochastic" => format!(
            "Stochastic metric: collect ≥{} replicates per arm (after warm-up). The verdict uses {} (auto-downgraded to Mann-Whitney + Cliff's δ, reported in parallel, if the samples fail a normality check — but the pre-registered criterion still decides).",
            required_samples_per_arm.unwrap_or(cfg.min_samples_per_arm),
            recommended_test
        ),
        "deterministic_distribution" => {
            "Distribution-valued deterministic metric (e.g. per-file complexity): submit one sample per measured unit, with the SAME `unit_key` (file path) on both arms so the paired Wilcoxon signed-rank test can match them.".to_string()
        }
        "deterministic_single" => {
            "Deterministic single-value metric: one measurement per arm suffices; the criterion is a threshold / relative-change rule, not a significance test.".to_string()
        }
        "observational" => {
            "Evidence-based hypothesis chain: record the predicted vs observed evidence for each hypothesis; the verdict is supported/falsified, no p-value. Prescribe the diagnostic commands/queries as the 'measurement kit'.".to_string()
        }
        _ => String::new(),
    });
    if matches!(kind, "feature_refactor" | "feature_addition") {
        notes.push(
            "Composite criterion: collect a separate arm/metric for each clause (e.g. perf no-regression via TOST, a structural metric via a `pgmcp_metric` run on each git ref, and tests-pass). Benjamini-Hochberg FDR is applied across the significance clauses.".to_string(),
        );
    }

    let data_schema = data_schema_of(&metric_nature);

    Protocol {
        kind: kind.to_string(),
        primary_metric: primary_metric.to_string(),
        unit: unit.map(str::to_string),
        predicted_direction: predicted_direction.to_string(),
        metric_nature,
        recommended_test,
        acceptance_criterion,
        required_samples_per_arm,
        warmup_runs,
        arms,
        correction: cfg.default_correction.clone(),
        data_schema,
        reproducibility_checklist: reproducibility_checklist(),
        notes,
    }
}

/// Classify the criterion's metric nature (drives sample sizing + warm-up).
fn metric_nature_of(c: &AcceptanceCriterion) -> &'static str {
    use AcceptanceCriterion as C;
    match c {
        // Need a sampling distribution + variance.
        C::WelchT { .. }
        | C::MannWhitneyU { .. }
        | C::BootstrapCiExcludes { .. }
        | C::Equivalence { .. } => "stochastic",
        // Paired before/after over the same units (N = #units, not replicates).
        C::WilcoxonSignedRank { .. } => "deterministic_distribution",
        C::Observational { .. } => "observational",
        C::AbsoluteThreshold { .. } | C::RelativeImprovement { .. } | C::EffectThreshold { .. } => {
            "deterministic_single"
        }
        C::AllOf(v) | C::AnyOf(v) => {
            // A composite is "stochastic" if any clause needs replicates,
            // else takes the nature of its first informative clause.
            if v.iter().any(|x| metric_nature_of(x) == "stochastic") {
                "stochastic"
            } else {
                v.first()
                    .map(metric_nature_of)
                    .unwrap_or("deterministic_single")
            }
        }
        C::Not(b) => metric_nature_of(b),
    }
}

fn recommended_test_of(c: &AcceptanceCriterion, default_test: &str) -> String {
    use AcceptanceCriterion as C;
    fn find(c: &C) -> Option<&'static str> {
        match c {
            C::WelchT { .. } => Some("welch_t"),
            C::MannWhitneyU { .. } => Some("mann_whitney_u"),
            C::WilcoxonSignedRank { .. } => Some("wilcoxon_signed_rank"),
            C::Equivalence { .. } => Some("tost_equivalence"),
            C::BootstrapCiExcludes { .. } => Some("bootstrap"),
            C::Observational { .. } => Some("observational"),
            C::AllOf(v) | C::AnyOf(v) => v.iter().find_map(find),
            C::Not(b) => find(b),
            _ => None,
        }
    }
    find(c)
        .map(str::to_string)
        .unwrap_or_else(|| default_test.to_string())
}

fn arms_of(kind: &str) -> Vec<ArmSpec> {
    match kind {
        "optimization" | "feature_refactor" | "feature_addition" | "other" => vec![
            ArmSpec {
                label: "control".to_string(),
                kind: "control".to_string(),
                description: "Baseline: the unchanged code / current main.".to_string(),
            },
            ArmSpec {
                label: "treatment".to_string(),
                kind: "treatment".to_string(),
                description: "The change under test.".to_string(),
            },
        ],
        // Evidence-based kinds collect observations, not control/treatment arms.
        _ => vec![ArmSpec {
            label: "observation".to_string(),
            kind: "baseline".to_string(),
            description: "Diagnostic evidence for the hypothesis chain.".to_string(),
        }],
    }
}

fn data_schema_of(metric_nature: &str) -> String {
    match metric_nature {
        "stochastic" => "experiment_record_measurement { arm: 'control'|'treatment', metric: <name>, samples: [f64; >=required_samples_per_arm], source: 'external_benchmark'|'pgmcp_metric'|'agent_scalar' }".to_string(),
        "deterministic_distribution" => "experiment_record_measurement { arm, metric, samples: [f64 per unit], unit_keys: [file paths aligned to samples], source: 'pgmcp_metric' }".to_string(),
        "deterministic_single" => "experiment_record_measurement { arm, metric, samples: [single f64], source }".to_string(),
        "observational" => "experiment_decide with an `observational` criterion carrying {prediction, observed, verdict}; no samples required.".to_string(),
        _ => "experiment_record_measurement { arm, metric, samples: [f64] }".to_string(),
    }
}

/// The benchmarking reproducibility requirements (from the global mandates).
fn reproducibility_checklist() -> Vec<String> {
    vec![
        "Pin each arm to a single CCD with `taskset -c <cores>` (or sched_setaffinity) to remove cross-CCD L3/NUMA variance.".to_string(),
        "Set every pinned core to the `performance` governor (`cpupower frequency-set -g performance`); record scaling_driver + boost state.".to_string(),
        "Record the full hardware string (first H1 of ~/.claude/hardware-specifications.md), `uname -a`, and the CPU model in host_meta.".to_string(),
        "Record the exact commands / tool invocations, the git SHA of each arm, and a fixed RNG seed.".to_string(),
        "Tee benchmark output to a file and run once — do not re-run to cherry-pick a favorable result.".to_string(),
        "Discard warm-up replicates; measure only after the series reaches steady state.".to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::acceptance::{AcceptanceCriterion, MarginSpec};

    fn cfg() -> ExperimentsConfig {
        ExperimentsConfig::default()
    }

    #[test]
    fn optimization_prescribes_stochastic_sample_size() {
        let crit = AcceptanceCriterion::default_optimization(true);
        let p = prescribe(
            "optimization",
            "p99_latency_ms",
            Some("ms"),
            "decrease",
            &crit,
            &cfg(),
            None,
        );
        assert_eq!(p.metric_nature, "stochastic");
        assert_eq!(p.recommended_test, "welch_t");
        // d=0.5 default → ~51 one-sided, floored at 30 → >= 30.
        let n = p.required_samples_per_arm.expect("stochastic needs N");
        assert!(n >= 30, "n={n}");
        assert_eq!(p.warmup_runs, 3);
        assert_eq!(p.arms.len(), 2);
        assert!(!p.reproducibility_checklist.is_empty());
    }

    #[test]
    fn investigation_prescribes_observational() {
        let crit = AcceptanceCriterion::Observational {
            prediction: "x".to_string(),
            observed: None,
            verdict: crate::stats::acceptance::ObsVerdict::Inconclusive,
        };
        let p = prescribe(
            "investigation",
            "root_cause",
            None,
            "none",
            &crit,
            &cfg(),
            None,
        );
        assert_eq!(p.metric_nature, "observational");
        assert!(p.required_samples_per_arm.is_none());
        assert_eq!(p.warmup_runs, 0);
        assert_eq!(p.arms.len(), 1);
    }

    #[test]
    fn refactor_composite_is_stochastic_with_fdr_note() {
        let crit = AcceptanceCriterion::AllOf(vec![
            AcceptanceCriterion::Equivalence {
                margin: MarginSpec::Percent { pct: 0.03 },
                alpha: 0.05,
            },
            AcceptanceCriterion::WilcoxonSignedRank {
                alpha: 0.05,
                tail: Tail::Less,
                min_effect: None,
            },
        ]);
        let p = prescribe(
            "feature_refactor",
            "lcom4",
            None,
            "decrease",
            &crit,
            &cfg(),
            None,
        );
        // Equivalence clause needs replicates → stochastic.
        assert_eq!(p.metric_nature, "stochastic");
        assert!(
            p.notes
                .iter()
                .any(|n| n.contains("FDR") || n.contains("Benjamini"))
        );
    }

    #[test]
    fn higher_expected_effect_needs_fewer_samples() {
        let crit = AcceptanceCriterion::default_optimization(false);
        let small = prescribe(
            "optimization",
            "m",
            None,
            "increase",
            &crit,
            &cfg(),
            Some(0.3),
        );
        let large = prescribe(
            "optimization",
            "m",
            None,
            "increase",
            &crit,
            &cfg(),
            Some(1.5),
        );
        assert!(small.required_samples_per_arm.unwrap() >= large.required_samples_per_arm.unwrap());
    }
}
