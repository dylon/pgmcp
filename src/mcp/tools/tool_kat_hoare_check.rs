//! `kat_hoare_check` — decide the propositional Hoare triple `{p} program {q}` over a
//! finite Boolean state space (Task #22 §4-A). In-process; no subprocess, no prattail.
//!
//! KAT (Kleene Algebra with Tests) validates `{p}·c·{q}` as `p·c·¬q ≡ 0`. For a
//! *propositional* program — guarded commands over Boolean variables — this is decided
//! exactly by running `c` as a state-set transformer from every `p`-state and checking
//! that every reachable final state satisfies `q`. Tests/guards are evaluated with the
//! hoisted `lling_llang::symbolic::kat_algebra::{BooleanTest, eval_test_public}`.

use std::collections::HashMap;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use lling_llang::symbolic::kat_algebra::{BooleanTest, eval_test_public};

use crate::context::SystemContext;
use crate::mcp::server::{BoolTestSpec, KatHoareCheckParams, KatStmt};
use crate::mcp::tools::sota_helpers::json_result;

fn to_test(s: &BoolTestSpec) -> BooleanTest {
    match s {
        BoolTestSpec::True => BooleanTest::True,
        BoolTestSpec::False => BooleanTest::False,
        BoolTestSpec::Atom { name } => BooleanTest::Atom(name.clone()),
        BoolTestSpec::Not { inner } => BooleanTest::Not(Box::new(to_test(inner))),
        BoolTestSpec::And { left, right } => {
            BooleanTest::And(Box::new(to_test(left)), Box::new(to_test(right)))
        }
        BoolTestSpec::Or { left, right } => {
            BooleanTest::Or(Box::new(to_test(left)), Box::new(to_test(right)))
        }
    }
}

/// A Boolean state as a position-indexed bit vector over `atoms`.
type State = Vec<bool>;

fn valuation(state: &State, atoms: &[String]) -> HashMap<String, bool> {
    atoms.iter().cloned().zip(state.iter().copied()).collect()
}

fn dedup(states: Vec<State>) -> Vec<State> {
    let mut seen = std::collections::HashSet::new();
    states.into_iter().filter(|s| seen.insert(s.clone())).collect()
}

pub async fn tool_kat_hoare_check(
    _ctx: &SystemContext,
    params: KatHoareCheckParams,
) -> Result<CallToolResult, McpError> {
    let atoms = &params.atoms;
    let n = atoms.len();
    if n > 20 {
        return Err(McpError::invalid_params(
            format!("too many atoms ({n}); the 2^n state space is bounded at 20"),
            None,
        ));
    }
    let index_of = |var: &str| atoms.iter().position(|a| a == var);
    let pre = to_test(&params.precond);
    let post = to_test(&params.postcond);

    // All valuations satisfying the precondition.
    let mut states: Vec<State> = (0..(1u32 << n))
        .map(|mask| (0..n).map(|i| (mask >> i) & 1 == 1).collect::<State>())
        .filter(|s| eval_test_public(&pre, &valuation(s, atoms)))
        .collect();

    // Run the program as a (non-deterministic) state-set transformer.
    for stmt in &params.program {
        match stmt {
            KatStmt::Assume { test } => {
                let t = to_test(test);
                states.retain(|s| eval_test_public(&t, &valuation(s, atoms)));
            }
            KatStmt::Assign { var, value } => {
                if let Some(i) = index_of(var) {
                    for s in &mut states {
                        s[i] = *value;
                    }
                }
                states = dedup(std::mem::take(&mut states));
            }
            KatStmt::Havoc { var } => {
                if let Some(i) = index_of(var) {
                    let mut ns = Vec::with_capacity(states.len() * 2);
                    for s in &states {
                        let mut s0 = s.clone();
                        s0[i] = false;
                        let mut s1 = s.clone();
                        s1[i] = true;
                        ns.push(s0);
                        ns.push(s1);
                    }
                    states = dedup(ns);
                }
            }
        }
    }

    // The triple holds iff every reachable final state satisfies the postcondition.
    let counterexample = states
        .iter()
        .find(|s| !eval_test_public(&post, &valuation(s, atoms)));
    let valid = counterexample.is_none();

    json_result(&json!({
        "valid": valid,
        "final_state_count": states.len(),
        "counterexample": counterexample.map(|s| valuation(s, atoms)),
        "method": "propositional KAT Hoare {p}·c·{q} = (p·c·¬q ≡ 0), finite Boolean state-set transformer",
    }))
}
