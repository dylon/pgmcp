//! Parameters for the native formal-verification (FV) MCP tools (Task #22 §4-A).
//!
//! Each tool runs `{pgmcp data | inline spec} → lling-llang/CSM engine → verdict`
//! entirely in-process (no subprocess, no prattail dependency). Param structs are
//! glob-re-exported by `params/mod.rs` so `crate::mcp::server::<Name>Params` resolves.

use rmcp::schemars;
use serde::Deserialize;

/// `protocol_soundness` — deadlock-freedom + progress for a CSM `GlobalType`.
///
/// By the proofs-as-plans result (`CsmDeadlockFreedom.v`, Task #22 §4-D), a
/// **well-formed** `GlobalType` is deadlock-free and has progress *by typing*, so this
/// tool decides soundness by checking MPST well-formedness — closing the gap that
/// otherwise needs an external model checker (pi + TLC).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProtocolSoundnessParams {
    /// The global protocol type as adjacent-tagged JSON (`{"type": …, "data": …}`),
    /// matching `csm::mpst::global::GlobalType`.
    #[schemars(description = "GlobalType as adjacent-tagged JSON ({\"type\":…,\"data\":…})")]
    pub global_type: serde_json::Value,
}

// ──────────────────────────────────────────────────────────────────────────────
// language_inclusion
// ──────────────────────────────────────────────────────────────────────────────

/// One symbolic transition: `from --[lo, hi)--> to` (a half-open integer-interval
/// guard, mapped to `IntervalPred::Range(lo, hi)`).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SfaEdgeSpec {
    /// Source state index.
    pub from: usize,
    /// Target state index.
    pub to: usize,
    /// Inclusive lower bound of the guard interval.
    pub lo: i64,
    /// Exclusive upper bound of the guard interval.
    pub hi: i64,
}

/// A Symbolic Finite Automaton over an integer-interval alphabet.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SfaSpec {
    /// Number of states (indices `0..num_states`).
    pub num_states: usize,
    /// Initial state index.
    pub initial: usize,
    /// Accepting state indices.
    pub accepting: Vec<usize>,
    /// Predicate-guarded transitions.
    pub transitions: Vec<SfaEdgeSpec>,
}

/// `language_inclusion` — decide `L(impl) ⊆ L(spec)` over Symbolic Finite Automata
/// (the merge-coordinator feature-preservation primitive), via
/// `is_empty(impl ∩ ¬spec)` on `lling_llang::symbolic`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LanguageInclusionParams {
    /// The implementation automaton.
    pub impl_sfa: SfaSpec,
    /// The specification automaton.
    pub spec_sfa: SfaSpec,
    /// Inclusive lower bound of the (finite, symbolic) domain. Default `0`.
    #[serde(default)]
    pub domain_min: i64,
    /// Exclusive upper bound of the domain. Default `1_114_112` (Unicode scalar range).
    #[serde(default = "default_domain_max")]
    pub domain_max: i64,
}

fn default_domain_max() -> i64 {
    1_114_112
}

// ──────────────────────────────────────────────────────────────────────────────
// presburger_decide
// ──────────────────────────────────────────────────────────────────────────────

/// The relation in a linear constraint `Σ cᵢ·xᵢ  R  rhs`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PresburgerRel {
    /// `≤`
    Le,
    /// `≥`
    Ge,
    /// `=`
    Eq,
}

/// A Presburger-arithmetic formula (mirrors `lling_llang::symbolic::presburger::PresburgerPred`).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PresburgerSpec {
    /// `⊤`
    True,
    /// `⊥`
    False,
    /// A linear (in)equality `Σ terms[i].1 · x_{terms[i].0}  rel  rhs`.
    Atom {
        /// `(variable_index, coefficient)` pairs.
        terms: Vec<(usize, i64)>,
        /// Right-hand side constant.
        rhs: i64,
        /// The relation.
        rel: PresburgerRel,
    },
    /// `a ∧ b`
    And {
        /// Left conjunct.
        left: Box<PresburgerSpec>,
        /// Right conjunct.
        right: Box<PresburgerSpec>,
    },
    /// `a ∨ b`
    Or {
        /// Left disjunct.
        left: Box<PresburgerSpec>,
        /// Right disjunct.
        right: Box<PresburgerSpec>,
    },
    /// `¬a`
    Not {
        /// The negated formula.
        inner: Box<PresburgerSpec>,
    },
    /// `∃ x_var. body`
    Exists {
        /// The existentially-bound variable index.
        var: usize,
        /// The body.
        body: Box<PresburgerSpec>,
    },
}

/// `presburger_decide` — decide satisfiability of a Presburger formula via the
/// automata-based decision procedure in `lling_llang::symbolic::presburger`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PresburgerDecideParams {
    /// The formula to decide.
    pub formula: PresburgerSpec,
    /// Bit width of the (bounded) integer encoding. Default `8`.
    #[serde(default = "default_bit_width")]
    pub bit_width: usize,
}

fn default_bit_width() -> usize {
    8
}

// ──────────────────────────────────────────────────────────────────────────────
// effect_verify
// ──────────────────────────────────────────────────────────────────────────────

/// `effect_verify` — does the set of effects reachable from a seed symbol conform to
/// an allowed-effect policy? The reachable effects (over the resolved-call subgraph,
/// `sema_helpers::effects`) form a set; conformance is the sound inclusion
/// `reachable ⊆ allowed`. Any reachable effect outside the policy is a violation
/// (with its shortest call depth as a witness).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EffectVerifyParams {
    /// The seed symbol id (root of the effect-reachability subgraph).
    pub seed_symbol_id: i64,
    /// The permitted effect kinds (e.g. `["pure", "alloc", "channel_send"]`).
    pub allowed_effects: Vec<String>,
    /// Maximum call-graph depth to explore. Default `8`.
    #[serde(default = "default_effect_depth")]
    pub max_depth: u32,
}

fn default_effect_depth() -> u32 {
    8
}

// ──────────────────────────────────────────────────────────────────────────────
// behavioral_check (CTL model-checking over a finite Kripke structure)
// ──────────────────────────────────────────────────────────────────────────────

/// A transition `from → to` of the labelled transition system.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LtsEdgeSpec {
    /// Source state index.
    pub from: usize,
    /// Target state index.
    pub to: usize,
}

/// A Computation-Tree-Logic formula (branching-time behavioral spec). `E` = "there
/// exists a path", `A` = "for all paths"; `X` next, `F` eventually, `G` globally,
/// `U` until.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum CtlFormula {
    /// `⊤`
    True,
    /// `⊥`
    False,
    /// Atomic proposition holding at a state.
    Atom {
        /// Proposition name.
        prop: String,
    },
    /// `¬a`
    Not {
        /// The negated formula.
        inner: Box<CtlFormula>,
    },
    /// `a ∧ b`
    And {
        /// Left conjunct.
        left: Box<CtlFormula>,
        /// Right conjunct.
        right: Box<CtlFormula>,
    },
    /// `a ∨ b`
    Or {
        /// Left disjunct.
        left: Box<CtlFormula>,
        /// Right disjunct.
        right: Box<CtlFormula>,
    },
    /// `EX a` — some successor satisfies `a`.
    Ex {
        /// The sub-formula.
        inner: Box<CtlFormula>,
    },
    /// `AX a` — every successor satisfies `a`.
    Ax {
        /// The sub-formula.
        inner: Box<CtlFormula>,
    },
    /// `EF a` — on some path, `a` eventually holds.
    Ef {
        /// The sub-formula.
        inner: Box<CtlFormula>,
    },
    /// `AF a` — on every path, `a` eventually holds.
    Af {
        /// The sub-formula.
        inner: Box<CtlFormula>,
    },
    /// `EG a` — on some path, `a` holds forever.
    Eg {
        /// The sub-formula.
        inner: Box<CtlFormula>,
    },
    /// `AG a` — on every path, `a` holds forever (invariant).
    Ag {
        /// The sub-formula.
        inner: Box<CtlFormula>,
    },
    /// `E[a U b]` — on some path, `a` until `b`.
    Eu {
        /// The "until" condition.
        left: Box<CtlFormula>,
        /// The eventual goal.
        right: Box<CtlFormula>,
    },
    /// `A[a U b]` — on every path, `a` until `b`.
    Au {
        /// The "until" condition.
        left: Box<CtlFormula>,
        /// The eventual goal.
        right: Box<CtlFormula>,
    },
}

/// `behavioral_check` — CTL model-checking of a finite labelled transition system.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BehavioralCheckParams {
    /// Number of states (`0..num_states`).
    pub num_states: usize,
    /// The state to check the formula at.
    pub initial: usize,
    /// Transitions of the LTS.
    pub transitions: Vec<LtsEdgeSpec>,
    /// Atomic propositions true at each state (indexed by state).
    pub labels: Vec<Vec<String>>,
    /// The CTL formula to check.
    pub formula: CtlFormula,
}

// ──────────────────────────────────────────────────────────────────────────────
// kat_hoare_check (propositional KAT Hoare logic over Boolean tests)
// ──────────────────────────────────────────────────────────────────────────────

/// A Boolean test (mirrors `lling_llang::symbolic::kat_algebra::BooleanTest`).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum BoolTestSpec {
    /// `⊤`
    True,
    /// `⊥`
    False,
    /// Atomic proposition.
    Atom {
        /// Proposition name.
        name: String,
    },
    /// `¬a`
    Not {
        /// The negated test.
        inner: Box<BoolTestSpec>,
    },
    /// `a ∧ b`
    And {
        /// Left conjunct.
        left: Box<BoolTestSpec>,
        /// Right conjunct.
        right: Box<BoolTestSpec>,
    },
    /// `a ∨ b`
    Or {
        /// Left disjunct.
        left: Box<BoolTestSpec>,
        /// Right disjunct.
        right: Box<BoolTestSpec>,
    },
}

/// One guarded-command statement of the KAT program.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum KatStmt {
    /// `assume(test)` — keep only states satisfying `test` (a KAT test/guard).
    Assume {
        /// The guard.
        test: BoolTestSpec,
    },
    /// `var := value` — set a Boolean variable.
    Assign {
        /// The variable.
        var: String,
        /// The new value.
        value: bool,
    },
    /// `havoc(var)` — set `var` non-deterministically (both values).
    Havoc {
        /// The variable.
        var: String,
    },
}

/// `kat_hoare_check` — decide the propositional Hoare triple `{precond} program
/// {postcond}` over a finite Boolean state space (`2^|atoms|` valuations).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KatHoareCheckParams {
    /// The Boolean variables (atomic propositions).
    pub atoms: Vec<String>,
    /// Precondition.
    pub precond: BoolTestSpec,
    /// The guarded-command program.
    pub program: Vec<KatStmt>,
    /// Postcondition.
    pub postcond: BoolTestSpec,
}
