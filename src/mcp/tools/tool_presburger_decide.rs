//! `presburger_decide` — decide satisfiability of a Presburger-arithmetic formula
//! via the automata-based decision procedure in `lling_llang::symbolic::presburger`
//! (Task #22 §4-A). In-process; no subprocess, no prattail.

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use lling_llang::symbolic::presburger::{LinearConstraint, PresburgerPred, is_satisfiable_nfa};

use crate::context::SystemContext;
use crate::mcp::server::{PresburgerDecideParams, PresburgerRel, PresburgerSpec};
use crate::mcp::tools::sota_helpers::json_result;

fn convert(spec: &PresburgerSpec) -> PresburgerPred {
    match spec {
        PresburgerSpec::True => PresburgerPred::True,
        PresburgerSpec::False => PresburgerPred::False,
        PresburgerSpec::Atom { terms, rhs, rel } => match rel {
            PresburgerRel::Le => PresburgerPred::Atom(LinearConstraint::new(terms.clone(), *rhs)),
            PresburgerRel::Ge => {
                PresburgerPred::Atom(LinearConstraint::from_gte(terms.clone(), *rhs))
            }
            // Eq ≡ (≤ ∧ ≥). Expressed as a conjunction to avoid the inherent
            // `LinearConstraint::eq` clashing with the `PartialEq::eq` trait method.
            PresburgerRel::Eq => PresburgerPred::And(
                Box::new(PresburgerPred::Atom(LinearConstraint::new(
                    terms.clone(),
                    *rhs,
                ))),
                Box::new(PresburgerPred::Atom(LinearConstraint::from_gte(
                    terms.clone(),
                    *rhs,
                ))),
            ),
        },
        PresburgerSpec::And { left, right } => {
            PresburgerPred::And(Box::new(convert(left)), Box::new(convert(right)))
        }
        PresburgerSpec::Or { left, right } => {
            PresburgerPred::Or(Box::new(convert(left)), Box::new(convert(right)))
        }
        PresburgerSpec::Not { inner } => PresburgerPred::Not(Box::new(convert(inner))),
        PresburgerSpec::Exists { var, body } => PresburgerPred::Exists {
            var: *var,
            body: Box::new(convert(body)),
        },
    }
}

pub async fn tool_presburger_decide(
    _ctx: &SystemContext,
    params: PresburgerDecideParams,
) -> Result<CallToolResult, McpError> {
    let pred = convert(&params.formula);
    let satisfiable = is_satisfiable_nfa(&pred, params.bit_width);
    json_result(&json!({
        "satisfiable": satisfiable,
        "bit_width": params.bit_width,
        "method": "presburger-nfa (automata-based decision procedure)",
    }))
}
