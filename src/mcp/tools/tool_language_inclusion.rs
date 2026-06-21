//! `language_inclusion` — decide `L(impl) ⊆ L(spec)` over Symbolic Finite Automata,
//! in-process via `lling_llang::symbolic` (Task #22 §4-A).
//!
//! This is the **merge-coordinator feature-preservation primitive**: "no exported
//! behavior is lost" reduces to language inclusion. Computed exactly as
//! `L(impl) ⊆ L(spec) ⟺ L(impl) ∩ ¬L(spec) = ∅` using the SFA `intersect`,
//! `complement`, and `is_empty` operations — no subprocess, no prattail.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use lling_llang::symbolic::{IntervalAlgebra, IntervalPred, SymbolicAutomaton};

use crate::context::SystemContext;
use crate::mcp::server::{LanguageInclusionParams, SfaSpec};
use crate::mcp::tools::sota_helpers::json_result;

fn build_sfa(spec: &SfaSpec, dmin: i64, dmax: i64) -> SymbolicAutomaton<IntervalAlgebra> {
    let mut a = SymbolicAutomaton::new(IntervalAlgebra::new(dmin, dmax));
    for i in 0..spec.num_states {
        a.add_state(spec.accepting.contains(&i), None);
    }
    if spec.initial < spec.num_states {
        a.set_initial(spec.initial);
    }
    for e in &spec.transitions {
        if e.from < spec.num_states && e.to < spec.num_states {
            a.add_transition(e.from, e.to, IntervalPred::Range(e.lo, e.hi));
        }
    }
    a
}

pub async fn tool_language_inclusion(
    _ctx: &SystemContext,
    params: LanguageInclusionParams,
) -> Result<CallToolResult, McpError> {
    let dmin = params.domain_min;
    let dmax = params.domain_max;
    let imp = build_sfa(&params.impl_sfa, dmin, dmax);
    let spc = build_sfa(&params.spec_sfa, dmin, dmax);

    // L(imp) ⊆ L(spc)  ⟺  L(imp) ∩ ¬L(spc) = ∅.
    let residual = imp.intersect(&spc.complement());
    let included = residual.is_empty();

    json_result(&json!({
        "included": included,
        // A non-empty residual is a *witness* word the impl accepts but the spec
        // forbids — the falsifiable counterexample (cf. ADR-012 Disc(P,M)=∅ ⇒ REJECT).
        "witness_exists": !included,
        "impl_states": imp.num_states(),
        "spec_states": spc.num_states(),
        "method": "sfa-inclusion (is_empty(impl ∩ ¬spec))",
    }))
}
