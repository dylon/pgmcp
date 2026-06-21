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
