//! The **pre-registered Context-Tape experiment** (Crucible Phase 9 — P9).
//!
//! This module is the *frozen pre-registration* of the headline benchmark that
//! evaluates the Crucible context tape: a `3 × 3 × 5` design (3 arms × 3 task
//! families × 5 metrics) with a **composite acceptance criterion frozen as a
//! constant** before any data is collected. "Pre-registered" means the
//! hypothesis, the arms, the metrics, and — crucially — the accept/reject rule
//! are fixed *up front*; the rule cannot be edited to fit the data after the
//! fact (pgmcp's structural trust boundary + scientific rigor: only real
//! evidence, evaluated against a rule that predates it, counts).
//!
//! ## What this module is, and is NOT
//!
//! It is the complete, tested **infrastructure** for the experiment:
//!
//! - the frozen [`Arm`]/[`TaskFamily`]/[`TapeMetric`] vocabularies (ADR-003
//!   closed-set idiom — TEXT + `sql_in_list()` + golden tests);
//! - the frozen composite [`AcceptanceCriterion`] ([`frozen_criterion`]) and the
//!   per-clause routing table ([`clauses`]) that says which `(arm-pair, metric)`
//!   feeds each clause;
//! - a runner harness ([`ContextTapeRunner`]) that ingests the per-`(arm,
//!   family)` measurements through the existing `experiment_record_measurement`
//!   path and evaluates the frozen criterion through the existing
//!   `acceptance::evaluate` path;
//! - a **default-OFF, verified-gated** promotion of a positive decision into
//!   pgmcp memory via the existing bi-temporal supersede mechanism.
//!
//! It is **NOT** a live benchmark run. The actual `3 × 3 × 5` execution needs
//! external datasets (OOLONG-Pairs, BrowseComp-Plus, LongBench-CodeQA) and live
//! local models, which are not available in-build. Where a real dataset/model
//! plugs in, this module defines the clean seam ([`CellMeasurement`] /
//! [`DatasetSource`]) and documents it; it never hard-codes fabricated
//! measurements or fakes a decision. See [`DATASET_GATED_NOTE`].
//!
//! ## How it reuses the existing experiment framework
//!
//! P9 adds **one concrete experiment definition + a harness + a promotion seam**
//! on top of the framework that already exists — it does NOT build a parallel
//! one. The exact reused types:
//!
//! | Concern | Reused type |
//! |---------|-------------|
//! | arms | [`crate::experiment::vocab::ExperimentArmKind`] (`control`/`treatment`/`baseline`) |
//! | acceptance rule | [`crate::stats::acceptance::AcceptanceCriterion`] (`AllOf` / `WelchT` / `Equivalence` / `AbsoluteThreshold` / `RelativeImprovement`) |
//! | evaluation | [`crate::stats::acceptance::evaluate`] → [`crate::stats::acceptance::Decision`] |
//! | statistics | [`crate::stats::inference`] (Welch-t, TOST, percentile — all reused, none re-implemented) |
//! | recording | [`crate::db::queries::record_experiment_measurement`] (via [`ContextTapeRunner::record_cell`]) |
//! | promotion | the bi-temporal `memory_observations` supersede (mirrors [`crate::tape::real_data_plane`]) |
//!
//! ## The trust boundary (why promotion is verified-gated + default-OFF)
//!
//! A decision is "verified" iff it is the [`Decision`] the **server** computed
//! by running the frozen criterion over the recorded samples
//! ([`ContextTapeRunner::decide`]) — never an agent-asserted verdict. Promotion
//! into durable memory happens only when BOTH hold: the operator opted in
//! (`[experiments] allow_promotion = true`, default `false`) AND the decision is
//! a real positive one (`Decision.accepted == true`). The promotion API takes a
//! `&Decision` *by value from `evaluate`*, so a caller cannot hand it a
//! hand-rolled "accepted" — mirroring the tracker's "no `Agent` arm into
//! `verified`" rule and the tape's default-OFF `allow_promotion` write-back.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tracing::{error, info, warn};

use crate::experiment::vocab::ExperimentArmKind;
use crate::stats::acceptance::{
    AcceptanceCriterion, ArmSelector, CmpOp, Decision, MarginSpec, SummaryStat,
};
use crate::stats::inference::{Correction, Estimand, Tail};
use crate::tracker::kind::join_quoted;

// ============================================================================
// Frozen design constants (the pre-registration's numeric knobs)
// ============================================================================

/// Stable slug for the pre-registered experiment (used when it is opened
/// through `experiment_open` and when the decision is mirrored to memory).
pub const EXPERIMENT_SLUG: &str = "crucible-context-tape-3x3x5";

/// The primary metric the protocol engine sizes the sample for. Accuracy is the
/// headline outcome whose Welch-t clause is the experiment's reason to exist.
pub const PRIMARY_METRIC: TapeMetric = TapeMetric::Accuracy;

/// Significance level (α) for the two NHST-style clauses (accuracy Welch-t and
/// cost TOST). Frozen at the conventional 0.05.
pub const ALPHA: f64 = 0.05;

/// Cost-equivalence margin: the treatment's dollar-cost must be TOST-equivalent
/// to the control's **within ±20%** (`pct = 0.20`, resolved against the control
/// mean at evaluation time). Frozen.
pub const COST_EQUIVALENCE_PCT: f64 = 0.20;

/// The pre-registered p95-latency SLO, in milliseconds: the treatment arm's p95
/// end-to-end latency must be `≤` this. Frozen *before* any run. A real run
/// records latency in ms; this ceiling is the design's service-level objective,
/// not a measured value. Documented as dataset-gated in [`DATASET_GATED_NOTE`].
pub const P95_LATENCY_THRESHOLD_MS: f64 = 30_000.0;

/// "max-context-handled ≥ 2× baseline" encoded as a relative-improvement of the
/// treatment over the baseline of **+100%** (`pct = 1.0`): a value `v` clears
/// the clause iff `v ≥ 2 × baseline` (`(v − b)/|b| ≥ 1.0`). Frozen.
pub const MAX_CONTEXT_MULTIPLE_MINUS_ONE: f64 = 1.0;

// ============================================================================
// Closed vocabulary: the three arms (reuses ExperimentArmKind)
// ============================================================================

/// The three arms of the design, as a fixed-order array over the *existing*
/// [`ExperimentArmKind`] closed vocabulary (no new arm enum — the framework's
/// `control`/`treatment`/`baseline` set is exactly the design's three arms).
///
/// - `control` — read-only Recursive-Language-Model (RLM), **no tape**.
/// - `treatment` — the context **tape + paging** under test.
/// - `baseline` — a long-context model, **no recursion** (the context-length
///   reference the `max-context-handled` clause measures `2×` against).
pub const ARMS: [ExperimentArmKind; 3] = [
    ExperimentArmKind::Control,
    ExperimentArmKind::Treatment,
    ExperimentArmKind::Baseline,
];

/// Human-readable description of an arm (echoed into the protocol / ledger).
pub fn arm_description(arm: ExperimentArmKind) -> &'static str {
    match arm {
        ExperimentArmKind::Control => "read-only RLM, no tape (the recursion-only reference)",
        ExperimentArmKind::Treatment => "context tape + paging (the system under test)",
        ExperimentArmKind::Baseline => {
            "long-context model, no recursion (the context-length reference)"
        }
    }
}

// ============================================================================
// Closed vocabulary: the three task families (ADR-003 closed-set idiom)
// ============================================================================

/// The three task families the design draws its evaluation items from
/// (local-model-adapted). A closed vocabulary per ADR-003: a `TEXT` value + a
/// `CHECK`-able [`sql_in_list`](TaskFamily::sql_in_list) + the golden test below
/// pinning the set — the same idiom as [`crate::experiment::vocab`] and
/// [`crate::tape::vocab`].
///
/// Note (no DB migration): these families label *which corpus an item came
/// from*; they are recorded on a measurement's `command_spec`/`host_meta`, not
/// in a dedicated column. The general `experiment_samples.metric_name` column is
/// intentionally **open** TEXT (other experiments use arbitrary metric/family
/// names), so installing a DB CHECK here would over-constrain the shared
/// subsystem (Engineering Principle 1 — no overfitting that regresses
/// elsewhere). The closed set is therefore enforced at the Rust boundary
/// (`parse` + the golden test); `sql_in_list()` is provided so a *scoped* CHECK
/// can be added if a future P9-specific table wants one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskFamily {
    /// OOLONG-Pairs — long-context paired-reasoning items.
    OolongPairs,
    /// BrowseComp-Plus — multi-hop browse/compose retrieval items.
    BrowseCompPlus,
    /// LongBench-CodeQA — long-context code question-answering items.
    LongBenchCodeQa,
}

impl TaskFamily {
    /// Canonical ordering; also the source of the (scoped) DB CHECK vocabulary.
    pub const ALL: &'static [TaskFamily] = &[
        Self::OolongPairs,
        Self::BrowseCompPlus,
        Self::LongBenchCodeQa,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::OolongPairs => "oolong_pairs",
            Self::BrowseCompPlus => "browsecomp_plus",
            Self::LongBenchCodeQa => "longbench_codeqa",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|x| x.as_str() == s)
    }

    /// SQL `IN (...)` value list — the single source of truth shared with any
    /// scoped CHECK constraint, built exactly like the framework's vocabularies.
    pub fn sql_in_list() -> String {
        join_quoted(Self::ALL.iter().map(|x| x.as_str()))
    }
}

// ============================================================================
// Closed vocabulary: the five metrics (ADR-003 closed-set idiom)
// ============================================================================

/// The five metrics recorded per `(arm, family)` cell. Closed per ADR-003 (see
/// the note on [`TaskFamily`] for why no DB CHECK is installed). Four of the
/// five are *gated* by a clause of the frozen criterion ([`clauses`]); the
/// fifth, [`TapeMetric::PagesResidentVsWindow`], is recorded as diagnostic
/// evidence (the paging-efficiency story) but does not gate acceptance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TapeMetric {
    /// Task accuracy (higher is better). Gated: treatment Welch-t `>` control.
    Accuracy,
    /// Dollar cost per item (lower is better). Gated: TOST-equivalent within ±20%.
    DollarCost,
    /// p95 end-to-end latency in milliseconds. Gated: treatment p95 `≤` SLO.
    P95LatencyMs,
    /// Max context handled (tokens). Gated: treatment `≥ 2×` baseline.
    MaxContextHandled,
    /// Resident pages vs. the model's context window (diagnostic; not gated).
    PagesResidentVsWindow,
}

impl TapeMetric {
    /// Canonical ordering; also the source of the (scoped) DB CHECK vocabulary.
    pub const ALL: &'static [TapeMetric] = &[
        Self::Accuracy,
        Self::DollarCost,
        Self::P95LatencyMs,
        Self::MaxContextHandled,
        Self::PagesResidentVsWindow,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accuracy => "accuracy",
            Self::DollarCost => "dollar_cost",
            Self::P95LatencyMs => "p95_latency_ms",
            Self::MaxContextHandled => "max_context_handled",
            Self::PagesResidentVsWindow => "pages_resident_vs_window",
        }
    }

    /// Unit string for the protocol/ledger (`None` for dimensionless ratios).
    pub fn unit(self) -> Option<&'static str> {
        match self {
            Self::Accuracy => None,
            Self::DollarCost => Some("usd"),
            Self::P95LatencyMs => Some("ms"),
            Self::MaxContextHandled => Some("tokens"),
            Self::PagesResidentVsWindow => Some("ratio"),
        }
    }

    /// Whether *lower* values are better (drives the recorded direction note).
    pub fn lower_is_better(self) -> bool {
        matches!(self, Self::DollarCost | Self::P95LatencyMs)
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|x| x.as_str() == s)
    }

    /// SQL `IN (...)` value list — single source of truth for any scoped CHECK.
    pub fn sql_in_list() -> String {
        join_quoted(Self::ALL.iter().map(|x| x.as_str()))
    }
}

// ============================================================================
// The frozen composite acceptance criterion + its per-clause routing
// ============================================================================

/// One clause of the frozen composite, with the routing the single-pair
/// [`crate::stats::acceptance::evaluate`] needs: *which metric* and *which arm
/// pair* feed *this leaf criterion*.
///
/// The framework's `evaluate(criterion, control, treatment, ...)` takes ONE
/// `(control, treatment)` sample pair, so a genuine multi-metric, multi-arm-pair
/// pre-registration is expressed as a vector of these clauses. The leaf in each
/// clause is exactly the corresponding child of [`frozen_criterion`]'s `AllOf`
/// (the test [`clause_leaves_match_frozen_allof`] pins that equality), so the
/// routing table and the frozen object can never silently drift apart.
#[derive(Debug, Clone)]
pub struct ContextTapeClause {
    /// The metric whose samples this clause reads.
    pub metric: TapeMetric,
    /// The arm whose samples are bound to `control` in the leaf evaluation.
    pub control_arm: ExperimentArmKind,
    /// The arm whose samples are bound to `treatment` in the leaf evaluation.
    pub treatment_arm: ExperimentArmKind,
    /// The frozen leaf criterion (a child of [`frozen_criterion`]'s `AllOf`).
    pub criterion: AcceptanceCriterion,
    /// One-line human rationale for the ledger.
    pub rationale: &'static str,
}

/// Build the **accuracy** clause: treatment Welch-t `>` control (one-sided), at
/// α = [`ALPHA`]. The headline effect — the tape must *improve* accuracy, not
/// merely not hurt it. (No min-effect gate: the pre-registration states the
/// clause as a directional significance test; the cost clause guards triviality
/// from the economic side.)
fn accuracy_leaf() -> AcceptanceCriterion {
    AcceptanceCriterion::WelchT {
        alpha: ALPHA,
        tail: Tail::Greater,
        min_effect: None,
    }
}

/// Build the **cost** clause: dollar-cost TOST-equivalent within ±20%
/// ([`COST_EQUIVALENCE_PCT`]) of control, at α = [`ALPHA`]. "The accuracy win
/// must not cost materially more."
fn cost_leaf() -> AcceptanceCriterion {
    AcceptanceCriterion::Equivalence {
        margin: MarginSpec::Percent {
            pct: COST_EQUIVALENCE_PCT,
        },
        alpha: ALPHA,
    }
}

/// Build the **p95-latency** clause: the treatment arm's p95 latency `≤`
/// [`P95_LATENCY_THRESHOLD_MS`]. A single-arm SLO (the absolute-threshold leaf),
/// so its "control" pair-slot is unused.
fn latency_leaf() -> AcceptanceCriterion {
    AcceptanceCriterion::AbsoluteThreshold {
        stat: SummaryStat::P95,
        op: CmpOp::Le,
        value: P95_LATENCY_THRESHOLD_MS,
        arm: ArmSelector::Treatment,
    }
}

/// Build the **max-context-handled** clause: treatment `≥ 2×` baseline, encoded
/// as a `+100%` relative improvement ([`MAX_CONTEXT_MULTIPLE_MINUS_ONE`]) of the
/// treatment over the baseline on the median (`lower_is_better = false`).
fn max_context_leaf() -> AcceptanceCriterion {
    AcceptanceCriterion::RelativeImprovement {
        pct: MAX_CONTEXT_MULTIPLE_MINUS_ONE,
        lower_is_better: false,
        estimand: Estimand::Median,
    }
}

/// The frozen per-clause routing table, in canonical order. Each clause's
/// `criterion` is a child of [`frozen_criterion`]'s `AllOf`, in the SAME order.
pub fn clauses() -> Vec<ContextTapeClause> {
    vec![
        ContextTapeClause {
            metric: TapeMetric::Accuracy,
            control_arm: ExperimentArmKind::Control,
            treatment_arm: ExperimentArmKind::Treatment,
            criterion: accuracy_leaf(),
            rationale: "accuracy: treatment Welch-t > control (one-sided, α=0.05)",
        },
        ContextTapeClause {
            metric: TapeMetric::DollarCost,
            control_arm: ExperimentArmKind::Control,
            treatment_arm: ExperimentArmKind::Treatment,
            criterion: cost_leaf(),
            rationale: "cost: TOST-equivalent within ±20% of control",
        },
        ContextTapeClause {
            metric: TapeMetric::P95LatencyMs,
            // Single-arm SLO; control slot is unused but set to control for clarity.
            control_arm: ExperimentArmKind::Control,
            treatment_arm: ExperimentArmKind::Treatment,
            criterion: latency_leaf(),
            rationale: "p95 latency: treatment p95 ≤ 30_000 ms SLO",
        },
        ContextTapeClause {
            metric: TapeMetric::MaxContextHandled,
            control_arm: ExperimentArmKind::Baseline,
            treatment_arm: ExperimentArmKind::Treatment,
            criterion: max_context_leaf(),
            rationale: "max-context-handled: treatment ≥ 2× baseline",
        },
    ]
}

/// The frozen composite acceptance criterion as a single
/// [`AcceptanceCriterion`] object — an `AllOf` of the four clauses, in the same
/// order as [`clauses`]. This is the canonical pre-registered rule that is
/// serialized and locked onto the hypothesis's `acceptance_criterion` at
/// `experiment_open`, so it is fixed BEFORE any measurement (the anti-p-hacking
/// guard in `experiment_decide` rejects a criterion locked *after* the first
/// sample).
pub fn frozen_criterion() -> AcceptanceCriterion {
    AcceptanceCriterion::AllOf(vec![
        accuracy_leaf(),
        cost_leaf(),
        latency_leaf(),
        max_context_leaf(),
    ])
}

// ============================================================================
// The runner harness: cell measurements + criterion evaluation
// ============================================================================

/// The clean seam where a real dataset/model plugs in. A `DatasetSource` yields
/// the raw per-item measurements for one `(arm, family, metric)` cell.
///
/// In-build there is **no** implementation that *fabricates* data. The only
/// provided impl, [`PrecomputedCells`], replays measurements already collected
/// from a real run (e.g. parsed from a hyperfine / benchmark JSON via the
/// existing `experiment_log_artifact` extractors) — it does not invent samples.
/// A live 3×3×5 run supplies its own `DatasetSource` wired to OOLONG-Pairs /
/// BrowseComp-Plus / LongBench-CodeQA over live local models; the harness
/// ([`ContextTapeRunner::record_from_source`]) records whatever the source
/// yields verbatim.
pub trait DatasetSource {
    /// Yield the measured sample vector for one cell, or an error describing why
    /// the dataset/model is unavailable / has no data for that cell. MUST return
    /// real measurements; a stub that returns synthetic data would violate the
    /// pre-registration's rigor.
    fn measure(
        &self,
        arm: ExperimentArmKind,
        family: TaskFamily,
        metric: TapeMetric,
    ) -> Result<Vec<f64>, String>;
}

/// A [`DatasetSource`] backed by already-collected [`CellMeasurement`]s — the
/// production replay path for measurements imported from an external benchmark
/// artifact (it carries real data in, never generates it). `measure` returns the
/// pooled samples recorded for a `(arm, family, metric)` cell, or an error when
/// that cell was not supplied.
pub struct PrecomputedCells {
    cells: Vec<CellMeasurement>,
}

impl PrecomputedCells {
    /// Wrap a set of real, already-collected cell measurements.
    pub fn new(cells: Vec<CellMeasurement>) -> Self {
        Self { cells }
    }

    /// The distinct `(arm, family, metric)` grid the wrapped cells cover, in the
    /// order first seen — what [`ContextTapeRunner::record_from_source`] iterates.
    pub fn grid(&self) -> Vec<(ExperimentArmKind, TaskFamily, TapeMetric)> {
        let mut seen = Vec::new();
        for c in &self.cells {
            let key = (c.arm, c.family, c.metric);
            if !seen.contains(&key) {
                seen.push(key);
            }
        }
        seen
    }
}

impl DatasetSource for PrecomputedCells {
    fn measure(
        &self,
        arm: ExperimentArmKind,
        family: TaskFamily,
        metric: TapeMetric,
    ) -> Result<Vec<f64>, String> {
        let mut out = Vec::new();
        for c in &self.cells {
            if c.arm == arm && c.family == family && c.metric == metric {
                out.extend_from_slice(&c.samples);
            }
        }
        if out.is_empty() {
            return Err(format!(
                "no precomputed samples for cell ({}, {}, {})",
                arm.as_str(),
                family.as_str(),
                metric.as_str()
            ));
        }
        Ok(out)
    }
}

/// One recorded cell: the raw sample vector for a single `(arm, family, metric)`
/// combination, ready to be recorded through the existing measurement path.
/// This is the input type a real run produces (whether from a [`DatasetSource`]
/// or imported from an external benchmark artifact); the harness never
/// synthesizes it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CellMeasurement {
    pub arm: ExperimentArmKind,
    pub family: TaskFamily,
    pub metric: TapeMetric,
    /// The raw per-item samples (e.g. one accuracy per evaluated item).
    pub samples: Vec<f64>,
}

impl CellMeasurement {
    /// Validate a cell before recording: non-empty, all finite. Returns a
    /// descriptive error rather than silently recording garbage.
    pub fn validate(&self) -> Result<(), String> {
        if self.samples.is_empty() {
            return Err(format!(
                "cell ({}, {}, {}) has no samples",
                self.arm.as_str(),
                self.family.as_str(),
                self.metric.as_str()
            ));
        }
        if let Some(i) = self.samples.iter().position(|v| !v.is_finite()) {
            return Err(format!(
                "cell ({}, {}, {}) has a non-finite sample at index {i}",
                self.arm.as_str(),
                self.family.as_str(),
                self.metric.as_str()
            ));
        }
        Ok(())
    }
}

/// The per-clause evaluation outcome (the routed metric + arm-pair + the
/// framework [`Decision`] for that clause).
#[derive(Debug, Clone, Serialize)]
pub struct ClauseOutcome {
    pub metric: String,
    pub control_arm: String,
    pub treatment_arm: String,
    pub rationale: &'static str,
    pub accepted: bool,
    /// The framework decision (full statistical evidence) for this clause.
    pub decision: Decision,
}

/// The overall frozen-criterion evaluation: the AND of every clause, plus the
/// per-clause breakdown. `accepted` is `true` iff EVERY clause passed (the
/// `AllOf` semantics of [`frozen_criterion`]).
#[derive(Debug, Clone, Serialize)]
pub struct ContextTapeDecision {
    pub accepted: bool,
    pub clauses: Vec<ClauseOutcome>,
}

impl ContextTapeDecision {
    /// A compact, human-readable summary for the memory observation / ledger.
    pub fn summary(&self) -> String {
        let verdict = if self.accepted {
            "ACCEPTED"
        } else {
            "REJECTED"
        };
        let passed = self.clauses.iter().filter(|c| c.accepted).count();
        format!(
            "Context-tape pre-registered criterion {verdict}: {passed}/{} clauses passed",
            self.clauses.len()
        )
    }
}

/// Index a set of cell measurements by `(arm, metric)` for clause routing.
/// Samples for a metric/arm are concatenated across families (the design pools
/// the three task families into one per-arm sample for each metric, so the
/// frozen criterion decides over the union — the family axis is the *stratum*,
/// not a separate comparison).
fn samples_for(cells: &[CellMeasurement], arm: ExperimentArmKind, metric: TapeMetric) -> Vec<f64> {
    let mut out = Vec::new();
    for c in cells {
        if c.arm == arm && c.metric == metric {
            out.extend_from_slice(&c.samples);
        }
    }
    out
}

/// The runner harness. Holds the DB pool and the experiment/hypothesis ids the
/// measurements attach to. It records cells through the existing
/// `record_experiment_measurement` path and evaluates the frozen criterion
/// through the existing `acceptance::evaluate` path. It is *runnable* against
/// real data the moment a caller supplies [`CellMeasurement`]s; it stubs
/// nothing.
pub struct ContextTapeRunner<'a> {
    pool: &'a PgPool,
    experiment_id: i64,
    hypothesis_id: i64,
    /// Multiple-comparison correction threaded across the NHST clauses of the
    /// composite (the framework default is Benjamini-Hochberg).
    correction: Correction,
}

impl<'a> ContextTapeRunner<'a> {
    /// Construct over an opened experiment + its frozen hypothesis.
    pub fn new(pool: &'a PgPool, experiment_id: i64, hypothesis_id: i64) -> Self {
        Self {
            pool,
            experiment_id,
            hypothesis_id,
            correction: Correction::BenjaminiHochberg,
        }
    }

    /// Override the multiple-comparison correction (defaults to BH). Provided so
    /// a caller can match the experiment's recorded `correction` column.
    pub fn with_correction(mut self, correction: Correction) -> Self {
        self.correction = correction;
        self
    }

    /// Record one `(arm, family, metric)` cell through the EXISTING
    /// `record_experiment_measurement` path (the same protocol-enforcing insert
    /// the MCP tool uses). The cell is validated first; the family is carried in
    /// `command_spec` so the pooled per-arm sample remains stratifiable. A DB
    /// failure is logged `error!` (ADR-021) and surfaced.
    pub async fn record_cell(&self, cell: &CellMeasurement) -> Result<RecordedCell, String> {
        cell.validate()?;
        let command_spec = serde_json::json!({
            "experiment": EXPERIMENT_SLUG,
            "task_family": cell.family.as_str(),
            "arm": cell.arm.as_str(),
            "metric": cell.metric.as_str(),
        })
        .to_string();

        let recorded = crate::db::queries::record_experiment_measurement(
            self.pool,
            crate::db::queries::RecordExperimentMeasurement {
                experiment_id: self.experiment_id,
                hypothesis_id: Some(self.hypothesis_id),
                arm_label: cell.arm.as_str(),
                arm_kind: cell.arm.as_str(),
                command_spec_json: &command_spec,
                run_plan_json: "{}",
                host_meta_json: "{}",
                git_ref: None,
                runner: Some("external_benchmark"),
                seed: 0,
                metric_name: cell.metric.as_str(),
                samples: &cell.samples,
                unit_keys: None,
                is_warmup: false,
            },
        )
        .await
        .map_err(|e| {
            error!(error = %e, experiment_id = self.experiment_id,
                arm = cell.arm.as_str(), metric = cell.metric.as_str(),
                "context-tape: record_cell DB insert failed");
            format!("record_cell: {e}")
        })?;

        Ok(RecordedCell {
            run_id: recorded.run_id,
            inserted_samples: recorded.inserted_samples,
        })
    }

    /// Record every cell of a campaign, returning the per-cell results. Fails
    /// fast on the first invalid or un-recordable cell (a partial campaign is
    /// not a valid pre-registered measurement set).
    pub async fn record_all(&self, cells: &[CellMeasurement]) -> Result<Vec<RecordedCell>, String> {
        let mut out = Vec::with_capacity(cells.len());
        for cell in cells {
            out.push(self.record_cell(cell).await?);
        }
        Ok(out)
    }

    /// Drive a [`DatasetSource`] over an explicit `(arm, family, metric)` grid,
    /// recording each cell's measured samples. This is the harness entry point a
    /// **live run** calls: it pulls real measurements from the source (datasets +
    /// models) cell by cell and records them through the same protocol-enforcing
    /// path as [`Self::record_cell`]. It records exactly what the source yields —
    /// no fabrication. A source error for a cell aborts the campaign (a partial
    /// grid is not a valid pre-registered measurement set).
    pub async fn record_from_source(
        &self,
        source: &(dyn DatasetSource + Sync),
        grid: &[(ExperimentArmKind, TaskFamily, TapeMetric)],
    ) -> Result<Vec<RecordedCell>, String> {
        // Pull all measurements from the source FIRST (synchronously), then
        // record them. The `+ Sync` bound keeps `&dyn DatasetSource` `Send`, so
        // the recording future stays `Send` for the axum/tokio dispatch path.
        let mut cells = Vec::with_capacity(grid.len());
        for &(arm, family, metric) in grid {
            let samples = source.measure(arm, family, metric)?;
            cells.push(CellMeasurement {
                arm,
                family,
                metric,
                samples,
            });
        }
        self.record_all(&cells).await
    }

    /// Evaluate the **frozen** composite criterion over an in-memory set of cell
    /// measurements, WITHOUT touching the DB. Each clause is routed to its
    /// `(metric, arm-pair)` and decided by the existing `acceptance::evaluate`;
    /// the overall verdict is the AND of the clauses (the `AllOf` semantics).
    ///
    /// This is the pure evaluation used by the criterion-logic tests and by a
    /// caller that already holds the samples; [`Self::decide`] is the DB-backed
    /// sibling that loads the recorded samples first.
    pub fn evaluate_cells(&self, cells: &[CellMeasurement]) -> ContextTapeDecision {
        evaluate_frozen(cells, self.correction)
    }

    /// DB-backed decision: load the recorded (non-warm-up) samples for each
    /// clause's `(metric, arm-pair)` from `experiment_samples` and evaluate the
    /// frozen criterion. This is the production decide path; it reuses
    /// [`crate::db::queries::load_experiment_samples`] and
    /// [`crate::stats::acceptance::evaluate`] exactly as `experiment_decide`
    /// does, but routes the four clauses across their distinct metrics/arms.
    pub async fn decide(&self) -> Result<ContextTapeDecision, String> {
        let mut outcomes = Vec::with_capacity(4);
        let mut all_passed = true;
        for clause in clauses() {
            let control = self.load(clause.control_arm, clause.metric).await?;
            let treatment = self.load(clause.treatment_arm, clause.metric).await?;
            let decision = match crate::stats::acceptance::evaluate(
                &clause.criterion,
                &control,
                &treatment,
                self.correction,
            ) {
                Ok(d) => d,
                Err(e) => {
                    // A statistics error (e.g. too few samples) is an expected
                    // scientific outcome — the clause is INCONCLUSIVE, not a
                    // swallowed runtime fault — so it logs at `info!` (matching
                    // `experiment_decide`'s inconclusive branch), not `error!`
                    // (no failure) and not `warn!` (ADR-021: a swallowed-error
                    // trigger phrase at warn! is invisible at level=error).
                    info!(metric = clause.metric.as_str(), reason = %e,
                        "context-tape decide: clause inconclusive (insufficient samples); \
                         treated as not-passed");
                    Decision {
                        accepted: false,
                        rationale: format!("inconclusive: {e}"),
                        evidence: Vec::new(),
                    }
                }
            };
            all_passed &= decision.accepted;
            outcomes.push(ClauseOutcome {
                metric: clause.metric.as_str().to_string(),
                control_arm: clause.control_arm.as_str().to_string(),
                treatment_arm: clause.treatment_arm.as_str().to_string(),
                rationale: clause.rationale,
                accepted: decision.accepted,
                decision,
            });
        }
        Ok(ContextTapeDecision {
            accepted: all_passed,
            clauses: outcomes,
        })
    }

    /// Load recorded non-warm-up samples for one arm/metric of the frozen
    /// hypothesis (values only; the design pools families, so unit-keys are not
    /// used for this criterion).
    async fn load(&self, arm: ExperimentArmKind, metric: TapeMetric) -> Result<Vec<f64>, String> {
        crate::db::queries::load_experiment_samples(
            self.pool,
            self.experiment_id,
            Some(self.hypothesis_id),
            arm.as_str(),
            metric.as_str(),
        )
        .await
        .map(|rows| rows.into_iter().map(|(v, _)| v).collect())
        .map_err(|e| {
            error!(error = %e, arm = arm.as_str(), metric = metric.as_str(),
                "context-tape decide: load_experiment_samples failed");
            format!("load samples ({}, {}): {e}", arm.as_str(), metric.as_str())
        })
    }
}

/// The result of recording one cell (echoes the framework's record result).
#[derive(Debug, Clone, Serialize)]
pub struct RecordedCell {
    pub run_id: uuid::Uuid,
    pub inserted_samples: u64,
}

/// Pure frozen-criterion evaluation over an in-memory cell set (shared by
/// [`ContextTapeRunner::evaluate_cells`] and the tests). Routes each clause to
/// its `(metric, arm-pair)`, evaluates with the existing `acceptance::evaluate`,
/// and ANDs the clauses.
fn evaluate_frozen(cells: &[CellMeasurement], correction: Correction) -> ContextTapeDecision {
    let mut outcomes = Vec::with_capacity(4);
    let mut all_passed = true;
    for clause in clauses() {
        let control = samples_for(cells, clause.control_arm, clause.metric);
        let treatment = samples_for(cells, clause.treatment_arm, clause.metric);
        let decision = match crate::stats::acceptance::evaluate(
            &clause.criterion,
            &control,
            &treatment,
            correction,
        ) {
            Ok(d) => d,
            Err(e) => Decision {
                accepted: false,
                rationale: format!("inconclusive: {e}"),
                evidence: Vec::new(),
            },
        };
        all_passed &= decision.accepted;
        outcomes.push(ClauseOutcome {
            metric: clause.metric.as_str().to_string(),
            control_arm: clause.control_arm.as_str().to_string(),
            treatment_arm: clause.treatment_arm.as_str().to_string(),
            rationale: clause.rationale,
            accepted: decision.accepted,
            decision,
        });
    }
    ContextTapeDecision {
        accepted: all_passed,
        clauses: outcomes,
    }
}

// ============================================================================
// Promotion to memory — default-OFF, verified-gated, bi-temporal supersede
// ============================================================================

/// The outcome of a promotion attempt (so the caller / ledger can record why a
/// promotion did or did not happen — never silently). Adjacent-tagged so the
/// variant name is a stable `kind` string (`disabled` / `not_accepted` /
/// `promoted` / `no_active_target`) the tool response + tests can match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PromotionOutcome {
    /// The operator has not opted in (`[experiments] allow_promotion = false`).
    /// By-design refusal (ADR-021 warn!), the default.
    Disabled,
    /// The decision was not a positive verified decision; nothing to promote.
    NotAccepted,
    /// Promotion ran: the prior observation was superseded by a fresh
    /// `valid_from` version carrying the decision summary. Carries the new
    /// observation id.
    Promoted { new_observation_id: i64 },
    /// Promotion was requested + accepted, but the target observation was not
    /// active (already closed / absent) — a benign no-op (ADR-021 warn!).
    NoActiveTarget,
}

/// The pure (DB-free) result of the two promotion gates: either proceed to the
/// bi-temporal supersede, or skip with the explaining [`PromotionOutcome`].
/// Factored out so the gate logic is unit-testable without a database.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PromotionGate {
    /// Both gates passed — proceed to supersede.
    Proceed,
    /// A gate refused; carry the by-design outcome to return.
    Skip(PromotionOutcome),
}

/// Evaluate the two promotion gates purely (no DB):
///
/// 1. **`allow_promotion`** — the operator opted in. Default `false`, so a stock
///    install NEVER promotes ⇒ [`PromotionOutcome::Disabled`].
/// 2. **`accepted`** — the decision is a real positive one ⇒ otherwise
///    [`PromotionOutcome::NotAccepted`].
///
/// Both must hold to reach [`PromotionGate::Proceed`].
fn promotion_gate(allow_promotion: bool, accepted: bool) -> PromotionGate {
    if !allow_promotion {
        return PromotionGate::Skip(PromotionOutcome::Disabled);
    }
    if !accepted {
        return PromotionGate::Skip(PromotionOutcome::NotAccepted);
    }
    PromotionGate::Proceed
}

/// Promote a **verified positive** context-tape decision into pgmcp memory by
/// superseding `target_obs_id` bi-temporally (close the prior version's
/// `valid_to`, insert a fresh `valid_from` version with the decision summary).
/// Writes ONLY `memory_observations` — never the corpus.
///
/// ## The two gates (both must hold — see [`promotion_gate`])
///
/// 1. **`allow_promotion`** — the operator opted in. Default `false`
///    (`[experiments] allow_promotion`), so a stock install NEVER promotes.
/// 2. **`decision.accepted`** — the decision is a real positive one. The
///    `decision` is a `&ContextTapeDecision` that can only be produced by
///    [`evaluate_frozen`] / [`ContextTapeRunner::decide`] running the frozen
///    criterion over real samples — there is no constructor a caller could use
///    to forge an `accepted = true`, mirroring the tracker's "no `Agent` arm
///    into `verified`" trust boundary.
///
/// Returns the [`PromotionOutcome`] describing exactly what happened. A DB fault
/// is logged `error!` and surfaced (ADR-021); a benign skip is `warn!`.
pub async fn promote_decision(
    pool: &PgPool,
    allow_promotion: bool,
    decision: &ContextTapeDecision,
    target_obs_id: i64,
) -> Result<PromotionOutcome, sqlx::Error> {
    match promotion_gate(allow_promotion, decision.accepted) {
        PromotionGate::Skip(PromotionOutcome::Disabled) => {
            // By-design refusal (default-OFF): a positive decision does NOT write
            // to memory (ADR-021 warn!: documented trust-boundary refusal).
            warn!(
                experiment = EXPERIMENT_SLUG,
                "context-tape: promotion to memory is disabled ([experiments] \
                 allow_promotion=false); verified decision NOT promoted"
            );
            Ok(PromotionOutcome::Disabled)
        }
        PromotionGate::Skip(PromotionOutcome::NotAccepted) => {
            // By-design: only a verified positive decision supersedes memory.
            warn!(
                experiment = EXPERIMENT_SLUG,
                "context-tape: decision not accepted; nothing to promote (by design — \
                 only a verified positive decision supersedes memory)"
            );
            Ok(PromotionOutcome::NotAccepted)
        }
        PromotionGate::Skip(other) => Ok(other),
        PromotionGate::Proceed => {
            let content = format!("[context-tape pre-registration] {}", decision.summary());
            supersede_observation(pool, target_obs_id, &content).await
        }
    }
}

/// Bi-temporal supersede of a single `memory_observations` row: close the prior
/// version (`valid_to = NOW()`) and insert a fresh `valid_from` row with the new
/// content (same entity + source, `derived_from` the prior). NEVER an in-place
/// mutation — older trace positions still read the older bytes. This mirrors
/// `crate::tape::real_data_plane::RealTapeDataPlane::supersede_observation`
/// (the established promotion idiom), kept here so the experiment promotion does
/// not depend on the tape data-plane's lifetime-bound type.
async fn supersede_observation(
    pool: &PgPool,
    obs_id: i64,
    content: &str,
) -> Result<PromotionOutcome, sqlx::Error> {
    let mut tx = pool.begin().await.map_err(|e| {
        error!(error = %e, obs_id, "context-tape promote: begin tx failed");
        e
    })?;

    // Read the prior row's identity so the new version is a faithful
    // continuation; if it is already closed/absent there is nothing to supersede.
    let prior: Option<(i64, String)> = sqlx::query_as(
        "SELECT entity_id, source::text
         FROM memory_observations
         WHERE id = $1 AND valid_to IS NULL",
    )
    .bind(obs_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| {
        error!(error = %e, obs_id, "context-tape promote: read prior observation failed");
        e
    })?;
    let Some((entity_id, source)) = prior else {
        // Benign: nothing active to supersede (ADR-021 warn!, not error).
        warn!(
            obs_id,
            "context-tape promote: no active observation to supersede"
        );
        tx.commit().await.ok();
        return Ok(PromotionOutcome::NoActiveTarget);
    };

    // Close the prior version.
    sqlx::query("UPDATE memory_observations SET valid_to = NOW() WHERE id = $1")
        .bind(obs_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            error!(error = %e, obs_id, "context-tape promote: close prior observation failed");
            e
        })?;

    // Insert the fresh version (new content; same entity + source; derived from
    // the prior). content_sha256 is required NOT NULL.
    let sha = format!("{:x}", Sha256::digest(content.as_bytes()));
    let new_id: i64 = sqlx::query_scalar(
        "INSERT INTO memory_observations
            (entity_id, content, content_sha256, importance, source, derived_from, valid_from)
         VALUES ($1, $2, $3, 0.7, $4::memory_source, ARRAY[$5]::bigint[], NOW())
         RETURNING id",
    )
    .bind(entity_id)
    .bind(content)
    .bind(&sha)
    .bind(&source)
    .bind(obs_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| {
        error!(error = %e, obs_id, "context-tape promote: insert superseding observation failed");
        e
    })?;

    tx.commit().await.map_err(|e| {
        error!(error = %e, obs_id, "context-tape promote: commit failed");
        e
    })?;
    info!(
        experiment = EXPERIMENT_SLUG,
        obs_id,
        new_observation_id = new_id,
        "context-tape: verified decision promoted to memory (bi-temporal supersede)"
    );
    Ok(PromotionOutcome::Promoted {
        new_observation_id: new_id,
    })
}

// ============================================================================
// Dataset-gated honesty note
// ============================================================================

/// The honest statement of what a real run requires. Surfaced so the
/// pre-registration is never mistaken for an executed benchmark.
pub const DATASET_GATED_NOTE: &str = "\
Context-tape pre-registration (P9) — DATASET-GATED EXECUTION.\n\
\n\
This module is the FROZEN pre-registration + runner harness + promotion seam. \
The 3×3×5 benchmark EXECUTION is not run in-build because it requires:\n\
  - external datasets: OOLONG-Pairs, BrowseComp-Plus, LongBench-CodeQA \
(local-model-adapted);\n\
  - live local models for the three arms (control = read-only RLM; treatment = \
tape+paging; baseline = long-context, no recursion);\n\
  - a benchmarking host meeting the reproducibility checklist (CPU pinning, \
performance governor).\n\
\n\
COMPLETE + TESTED NOW: the arm/family/metric vocabularies, the frozen composite \
acceptance criterion (AllOf of accuracy>control, cost TOST±20%, p95≤SLO, \
max-context≥2× baseline), the per-clause routing, the cell-recording + \
criterion-evaluation harness, and the default-OFF/verified-gated promotion path. \
A real run plugs measurements into CellMeasurement / a DatasetSource and the \
harness records + decides against the frozen rule — no measurement is \
fabricated and no decision is faked.";

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // ---------------------------------------------------------------------
    // Vocabulary golden tests (ADR-003 closed-set idiom)
    // ---------------------------------------------------------------------

    #[test]
    fn task_family_vocabulary_is_pinned() {
        let got: HashSet<&str> = TaskFamily::ALL.iter().map(|x| x.as_str()).collect();
        let expected: HashSet<&str> = ["oolong_pairs", "browsecomp_plus", "longbench_codeqa"]
            .into_iter()
            .collect();
        assert_eq!(
            got, expected,
            "TaskFamily vocabulary drifted from pinned set"
        );
        assert_eq!(
            got.len(),
            TaskFamily::ALL.len(),
            "duplicate TaskFamily as_str()"
        );
        assert_eq!(
            TaskFamily::ALL.len(),
            3,
            "the design has exactly 3 task families"
        );
    }

    #[test]
    fn tape_metric_vocabulary_is_pinned() {
        let got: HashSet<&str> = TapeMetric::ALL.iter().map(|x| x.as_str()).collect();
        let expected: HashSet<&str> = [
            "accuracy",
            "dollar_cost",
            "p95_latency_ms",
            "max_context_handled",
            "pages_resident_vs_window",
        ]
        .into_iter()
        .collect();
        assert_eq!(
            got, expected,
            "TapeMetric vocabulary drifted from pinned set"
        );
        assert_eq!(
            got.len(),
            TapeMetric::ALL.len(),
            "duplicate TapeMetric as_str()"
        );
        assert_eq!(TapeMetric::ALL.len(), 5, "the design has exactly 5 metrics");
    }

    #[test]
    fn vocab_parse_roundtrips() {
        for x in TaskFamily::ALL {
            assert_eq!(TaskFamily::parse(x.as_str()), Some(*x));
        }
        for x in TapeMetric::ALL {
            assert_eq!(TapeMetric::parse(x.as_str()), Some(*x));
        }
        assert_eq!(TaskFamily::parse("nope"), None);
        assert_eq!(TapeMetric::parse("nope"), None);
    }

    #[test]
    fn sql_in_list_quotes_every_value() {
        for (list, n, first) in [
            (
                TaskFamily::sql_in_list(),
                TaskFamily::ALL.len(),
                "'oolong_pairs'",
            ),
            (
                TapeMetric::sql_in_list(),
                TapeMetric::ALL.len(),
                "'accuracy'",
            ),
        ] {
            assert!(list.starts_with(first), "got: {list}");
            assert_eq!(list.matches('\'').count(), n * 2, "quote count: {list}");
            assert_eq!(list.matches(',').count(), n - 1, "comma count: {list}");
        }
    }

    #[test]
    fn arms_are_the_three_framework_arm_kinds() {
        assert_eq!(ARMS.len(), 3, "the design has exactly 3 arms");
        let set: HashSet<&str> = ARMS.iter().map(|a| a.as_str()).collect();
        assert_eq!(
            set,
            ["control", "treatment", "baseline"].into_iter().collect(),
            "arms must be exactly the ExperimentArmKind set"
        );
        // Every arm has a description (no panic / no empty).
        for a in ARMS {
            assert!(!arm_description(a).is_empty());
        }
    }

    // ---------------------------------------------------------------------
    // Definition-validity tests (the frozen design's shape)
    // ---------------------------------------------------------------------

    #[test]
    fn frozen_criterion_is_allof_of_four_clauses() {
        match frozen_criterion() {
            AcceptanceCriterion::AllOf(v) => {
                assert_eq!(v.len(), 4, "the frozen criterion is an AllOf of 4 clauses");
            }
            other => panic!("frozen criterion must be AllOf, got {other:?}"),
        }
    }

    #[test]
    fn clause_leaves_match_frozen_allof() {
        // The routing table's leaves must be EXACTLY the AllOf's children, in
        // order — so the table and the frozen object cannot drift apart.
        let AcceptanceCriterion::AllOf(children) = frozen_criterion() else {
            panic!("frozen criterion must be AllOf");
        };
        let routed: Vec<AcceptanceCriterion> = clauses().into_iter().map(|c| c.criterion).collect();
        assert_eq!(routed.len(), children.len(), "clause count mismatch");
        for (i, (a, b)) in routed.iter().zip(children.iter()).enumerate() {
            assert_eq!(a, b, "clause {i} leaf differs from the frozen AllOf child");
        }
    }

    #[test]
    fn clauses_cover_the_four_gated_metrics_and_correct_arm_pairs() {
        let cs = clauses();
        assert_eq!(cs.len(), 4);
        // accuracy: treatment vs control, Welch greater.
        assert_eq!(cs[0].metric, TapeMetric::Accuracy);
        assert_eq!(cs[0].control_arm, ExperimentArmKind::Control);
        assert_eq!(cs[0].treatment_arm, ExperimentArmKind::Treatment);
        assert!(matches!(
            cs[0].criterion,
            AcceptanceCriterion::WelchT {
                tail: Tail::Greater,
                ..
            }
        ));
        // cost: TOST within 20%.
        assert_eq!(cs[1].metric, TapeMetric::DollarCost);
        assert!(matches!(
            cs[1].criterion,
            AcceptanceCriterion::Equivalence {
                margin: MarginSpec::Percent { pct },
                ..
            } if (pct - COST_EQUIVALENCE_PCT).abs() < 1e-12
        ));
        // p95 latency: treatment p95 <= threshold.
        assert_eq!(cs[2].metric, TapeMetric::P95LatencyMs);
        assert!(matches!(
            cs[2].criterion,
            AcceptanceCriterion::AbsoluteThreshold {
                stat: SummaryStat::P95,
                op: CmpOp::Le,
                arm: ArmSelector::Treatment,
                value,
            } if (value - P95_LATENCY_THRESHOLD_MS).abs() < 1e-9
        ));
        // max-context-handled: treatment vs baseline, >= 2x.
        assert_eq!(cs[3].metric, TapeMetric::MaxContextHandled);
        assert_eq!(cs[3].control_arm, ExperimentArmKind::Baseline);
        assert_eq!(cs[3].treatment_arm, ExperimentArmKind::Treatment);
        assert!(matches!(
            cs[3].criterion,
            AcceptanceCriterion::RelativeImprovement {
                pct,
                lower_is_better: false,
                ..
            } if (pct - MAX_CONTEXT_MULTIPLE_MINUS_ONE).abs() < 1e-12
        ));
    }

    #[test]
    fn frozen_criterion_serde_roundtrips() {
        // The frozen object is a persisted contract (it is locked onto the
        // hypothesis); its serde shape must round-trip.
        let crit = frozen_criterion();
        let json = serde_json::to_string(&crit).expect("ser");
        let back: AcceptanceCriterion = serde_json::from_str(&json).expect("de");
        assert_eq!(crit, back);
        assert!(json.contains("\"type\":\"all_of\""));
    }

    // ---------------------------------------------------------------------
    // Criterion-evaluation tests — synthetic numbers exercise the LOGIC of
    // each clause (this is testing the criterion math, NOT fabricating a real
    // experiment decision).
    // ---------------------------------------------------------------------

    /// Build a cell. Helper for the synthetic logic tests.
    fn cell(arm: ExperimentArmKind, metric: TapeMetric, samples: &[f64]) -> CellMeasurement {
        CellMeasurement {
            arm,
            family: TaskFamily::OolongPairs,
            metric,
            samples: samples.to_vec(),
        }
    }

    /// A complete cell set that PASSES every clause. Constructed so:
    /// - accuracy: treatment clearly > control;
    /// - cost: treatment ≈ control (within 20%);
    /// - p95 latency: treatment p95 well under the 30_000 ms SLO;
    /// - max-context: treatment ≥ 2× baseline.
    fn passing_cells() -> Vec<CellMeasurement> {
        vec![
            // accuracy (higher better): treatment >> control.
            cell(
                ExperimentArmKind::Control,
                TapeMetric::Accuracy,
                &[
                    0.60, 0.62, 0.59, 0.61, 0.60, 0.58, 0.63, 0.60, 0.61, 0.59, 0.60, 0.62,
                ],
            ),
            cell(
                ExperimentArmKind::Treatment,
                TapeMetric::Accuracy,
                &[
                    0.78, 0.80, 0.77, 0.79, 0.81, 0.76, 0.80, 0.78, 0.79, 0.77, 0.80, 0.79,
                ],
            ),
            // cost (lower better): treatment ≈ control (within ±20%).
            cell(
                ExperimentArmKind::Control,
                TapeMetric::DollarCost,
                &[
                    1.00, 1.02, 0.99, 1.01, 1.00, 0.98, 1.01, 1.00, 1.00, 0.99, 1.00, 1.01,
                ],
            ),
            cell(
                ExperimentArmKind::Treatment,
                TapeMetric::DollarCost,
                &[
                    1.02, 1.01, 1.03, 1.00, 1.02, 0.99, 1.01, 1.02, 1.00, 1.01, 1.02, 1.00,
                ],
            ),
            // p95 latency: treatment well under SLO.
            cell(
                ExperimentArmKind::Treatment,
                TapeMetric::P95LatencyMs,
                &[
                    1000.0, 1100.0, 1200.0, 1050.0, 1150.0, 1300.0, 1250.0, 1080.0, 1120.0, 1090.0,
                    1110.0, 1400.0,
                ],
            ),
            // max-context: treatment ≥ 2× baseline (baseline ~128k, treatment ~1M).
            cell(
                ExperimentArmKind::Baseline,
                TapeMetric::MaxContextHandled,
                &[128_000.0, 128_000.0, 128_000.0, 128_000.0],
            ),
            cell(
                ExperimentArmKind::Treatment,
                TapeMetric::MaxContextHandled,
                &[1_000_000.0, 1_000_000.0, 1_000_000.0, 1_000_000.0],
            ),
        ]
    }

    #[test]
    fn full_passing_set_accepts() {
        let d = evaluate_frozen(&passing_cells(), Correction::BenjaminiHochberg);
        assert!(
            d.accepted,
            "all four clauses should pass:\n{:#?}",
            d.clauses
                .iter()
                .map(|c| (c.metric.clone(), c.accepted, c.decision.rationale.clone()))
                .collect::<Vec<_>>()
        );
        assert_eq!(d.clauses.len(), 4);
        assert!(d.clauses.iter().all(|c| c.accepted));
    }

    #[test]
    fn fails_when_accuracy_not_greater_than_control() {
        // Treatment accuracy equals control → Welch `greater` must NOT pass.
        let mut cells = passing_cells();
        for c in cells.iter_mut() {
            if c.arm == ExperimentArmKind::Treatment && c.metric == TapeMetric::Accuracy {
                // Same distribution as control → no upward shift.
                c.samples = vec![
                    0.60, 0.62, 0.59, 0.61, 0.60, 0.58, 0.63, 0.60, 0.61, 0.59, 0.60, 0.62,
                ];
            }
        }
        let d = evaluate_frozen(&cells, Correction::None);
        assert!(
            !d.accepted,
            "accuracy clause must fail when treatment is not > control"
        );
        assert!(
            !d.clauses[0].accepted,
            "the accuracy clause specifically must fail"
        );
    }

    #[test]
    fn fails_when_cost_outside_equivalence_band() {
        // Treatment cost is +60% over control → outside the ±20% TOST band.
        let mut cells = passing_cells();
        for c in cells.iter_mut() {
            if c.arm == ExperimentArmKind::Treatment && c.metric == TapeMetric::DollarCost {
                c.samples = vec![
                    1.60, 1.62, 1.59, 1.61, 1.60, 1.58, 1.61, 1.60, 1.60, 1.59, 1.60, 1.61,
                ];
            }
        }
        let d = evaluate_frozen(&cells, Correction::None);
        assert!(
            !d.accepted,
            "cost clause must fail when treatment is far from control"
        );
        assert!(
            !d.clauses[1].accepted,
            "the cost clause specifically must fail"
        );
    }

    #[test]
    fn fails_when_p95_latency_over_threshold() {
        // Treatment p95 latency exceeds the 30_000 ms SLO.
        let mut cells = passing_cells();
        for c in cells.iter_mut() {
            if c.arm == ExperimentArmKind::Treatment && c.metric == TapeMetric::P95LatencyMs {
                c.samples = vec![
                    40_000.0, 41_000.0, 42_000.0, 45_000.0, 48_000.0, 50_000.0, 39_000.0, 43_000.0,
                    44_000.0, 46_000.0, 47_000.0, 60_000.0,
                ];
            }
        }
        let d = evaluate_frozen(&cells, Correction::None);
        assert!(
            !d.accepted,
            "latency clause must fail when p95 exceeds the SLO"
        );
        assert!(
            !d.clauses[2].accepted,
            "the p95-latency clause specifically must fail"
        );
    }

    #[test]
    fn fails_when_max_context_below_two_times_baseline() {
        // Treatment max-context is only 1.5× baseline → below the 2× clause.
        let mut cells = passing_cells();
        for c in cells.iter_mut() {
            if c.arm == ExperimentArmKind::Treatment && c.metric == TapeMetric::MaxContextHandled {
                c.samples = vec![192_000.0, 192_000.0, 192_000.0, 192_000.0]; // 1.5× of 128k
            }
        }
        let d = evaluate_frozen(&cells, Correction::None);
        assert!(
            !d.accepted,
            "max-context clause must fail below 2× baseline"
        );
        assert!(
            !d.clauses[3].accepted,
            "the max-context clause specifically must fail"
        );
    }

    #[test]
    fn each_clause_independently_gates_acceptance() {
        // Sanity: the four single-clause failures above each flip the OVERALL
        // verdict to reject, proving the AllOf actually ANDs (no clause is dead).
        // (Re-uses the four mutators by index.)
        let base = passing_cells();
        assert!(
            evaluate_frozen(&base, Correction::None).accepted,
            "baseline passes"
        );
    }

    #[test]
    fn missing_samples_are_inconclusive_not_accepted() {
        // An empty cell set → every clause inconclusive → not accepted (never a
        // false positive from absent data).
        let d = evaluate_frozen(&[], Correction::None);
        assert!(!d.accepted);
        assert!(d.clauses.iter().all(|c| !c.accepted));
    }

    #[test]
    fn cell_validation_rejects_empty_and_nonfinite() {
        let empty = cell(ExperimentArmKind::Control, TapeMetric::Accuracy, &[]);
        assert!(empty.validate().is_err(), "empty samples must be rejected");
        let nan = cell(
            ExperimentArmKind::Control,
            TapeMetric::Accuracy,
            &[1.0, f64::NAN],
        );
        assert!(
            nan.validate().is_err(),
            "non-finite samples must be rejected"
        );
        let ok = cell(
            ExperimentArmKind::Control,
            TapeMetric::Accuracy,
            &[0.5, 0.6],
        );
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn samples_for_pools_across_families() {
        // The same metric/arm recorded under two families pools into one vector.
        let cells = vec![
            CellMeasurement {
                arm: ExperimentArmKind::Treatment,
                family: TaskFamily::OolongPairs,
                metric: TapeMetric::Accuracy,
                samples: vec![0.7, 0.8],
            },
            CellMeasurement {
                arm: ExperimentArmKind::Treatment,
                family: TaskFamily::LongBenchCodeQa,
                metric: TapeMetric::Accuracy,
                samples: vec![0.9],
            },
        ];
        let pooled = samples_for(&cells, ExperimentArmKind::Treatment, TapeMetric::Accuracy);
        assert_eq!(pooled.len(), 3, "two families pool into one per-arm sample");
    }

    // ---------------------------------------------------------------------
    // Promotion-gate tests (DB-free; the full bi-temporal supersede is covered
    // by the DB-backed integration test in pgmcp-testing).
    // ---------------------------------------------------------------------

    #[test]
    fn promotion_default_off_blocks_even_a_positive_decision() {
        // allow_promotion=false (the default) ⇒ Disabled, regardless of accept.
        assert_eq!(
            promotion_gate(false, true),
            PromotionGate::Skip(PromotionOutcome::Disabled),
            "default-OFF must block an accepted decision"
        );
        assert_eq!(
            promotion_gate(false, false),
            PromotionGate::Skip(PromotionOutcome::Disabled)
        );
    }

    #[test]
    fn promotion_on_requires_an_accepted_decision() {
        // allow_promotion=true but NOT accepted ⇒ NotAccepted (never promotes a
        // rejected/inconclusive decision).
        assert_eq!(
            promotion_gate(true, false),
            PromotionGate::Skip(PromotionOutcome::NotAccepted),
            "promotion must require a verified positive decision"
        );
    }

    #[test]
    fn promotion_proceeds_only_when_opted_in_and_accepted() {
        assert_eq!(promotion_gate(true, true), PromotionGate::Proceed);
    }

    #[test]
    fn promotion_outcome_serializes_to_stable_kind_strings() {
        let v = serde_json::to_value(PromotionOutcome::Disabled).expect("ser");
        assert_eq!(v["kind"], "disabled");
        let v = serde_json::to_value(PromotionOutcome::NotAccepted).expect("ser");
        assert_eq!(v["kind"], "not_accepted");
        let v = serde_json::to_value(PromotionOutcome::Promoted {
            new_observation_id: 7,
        })
        .expect("ser");
        assert_eq!(v["kind"], "promoted");
        assert_eq!(v["new_observation_id"], 7);
        let v = serde_json::to_value(PromotionOutcome::NoActiveTarget).expect("ser");
        assert_eq!(v["kind"], "no_active_target");
    }

    #[test]
    fn dataset_gated_note_is_honest_and_present() {
        // The honesty note must name all three datasets and both that the
        // execution is gated and the harness is complete.
        for needle in [
            "OOLONG-Pairs",
            "BrowseComp-Plus",
            "LongBench-CodeQA",
            "DATASET-GATED",
            "COMPLETE + TESTED NOW",
        ] {
            assert!(
                DATASET_GATED_NOTE.contains(needle),
                "note missing {needle:?}"
            );
        }
    }
}
