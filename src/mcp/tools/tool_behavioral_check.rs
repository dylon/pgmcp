//! `behavioral_check` — CTL model-checking of a finite labelled transition system
//! (Task #22 §4-A). In-process; no subprocess, no prattail.
//!
//! Implements the standard Clarke–Emerson–Sistla CTL labelling algorithm: each
//! sub-formula is mapped to the exact set of states satisfying it (atomic sets,
//! Boolean set ops, the `EX`/`AX` pre-image, and least/greatest fixpoints for
//! `EF/AF/EG/AG/EU/AU`). The branching-time complement to the SMT/SFA/Presburger
//! tools — a complete, decidable behavioral verifier over a finite Kripke structure.

use std::collections::HashSet;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::server::{BehavioralCheckParams, CtlFormula};
use crate::mcp::tools::sota_helpers::json_result;

/// Pre-image: `{s | ∃t∈succ(s). t∈z}` (`forall=false`) or `{s | ∀t∈succ(s). t∈z}`
/// (`forall=true`). A state with no successors is vacuously in the `∀` pre-image.
fn pre(z: &HashSet<usize>, n: usize, succ: &[Vec<usize>], forall: bool) -> HashSet<usize> {
    (0..n)
        .filter(|&s| {
            if forall {
                succ[s].iter().all(|t| z.contains(t))
            } else {
                succ[s].iter().any(|t| z.contains(t))
            }
        })
        .collect()
}

/// Least fixpoint `µZ. base ∪ pre(Z)` (for `EF`/`AF`).
fn lfp(base: &HashSet<usize>, n: usize, succ: &[Vec<usize>], forall: bool) -> HashSet<usize> {
    let mut z: HashSet<usize> = HashSet::new();
    loop {
        let mut nz = base.clone();
        nz.extend(pre(&z, n, succ, forall));
        if nz == z {
            return z;
        }
        z = nz;
    }
}

/// Greatest fixpoint `νZ. base ∩ pre(Z)` (for `EG`/`AG`).
fn gfp(base: &HashSet<usize>, n: usize, succ: &[Vec<usize>], forall: bool) -> HashSet<usize> {
    let mut z: HashSet<usize> = (0..n).collect();
    loop {
        let p = pre(&z, n, succ, forall);
        let nz: HashSet<usize> = base.intersection(&p).copied().collect();
        if nz == z {
            return z;
        }
        z = nz;
    }
}

/// Until least fixpoint `µZ. b ∪ (a ∩ pre(Z))` (for `EU`/`AU`).
fn until(
    a: &HashSet<usize>,
    b: &HashSet<usize>,
    n: usize,
    succ: &[Vec<usize>],
    forall: bool,
) -> HashSet<usize> {
    let mut z: HashSet<usize> = HashSet::new();
    loop {
        let p = pre(&z, n, succ, forall);
        let ap: HashSet<usize> = a.intersection(&p).copied().collect();
        let mut nz = b.clone();
        nz.extend(ap);
        if nz == z {
            return z;
        }
        z = nz;
    }
}

fn sat(
    f: &CtlFormula,
    n: usize,
    succ: &[Vec<usize>],
    labels: &[HashSet<String>],
) -> HashSet<usize> {
    use CtlFormula::*;
    match f {
        True => (0..n).collect(),
        False => HashSet::new(),
        Atom { prop } => (0..n).filter(|&s| labels[s].contains(prop)).collect(),
        Not { inner } => {
            let a = sat(inner, n, succ, labels);
            (0..n).filter(|s| !a.contains(s)).collect()
        }
        And { left, right } => {
            let a = sat(left, n, succ, labels);
            let b = sat(right, n, succ, labels);
            a.intersection(&b).copied().collect()
        }
        Or { left, right } => {
            let a = sat(left, n, succ, labels);
            let b = sat(right, n, succ, labels);
            a.union(&b).copied().collect()
        }
        Ex { inner } => pre(&sat(inner, n, succ, labels), n, succ, false),
        Ax { inner } => pre(&sat(inner, n, succ, labels), n, succ, true),
        Ef { inner } => lfp(&sat(inner, n, succ, labels), n, succ, false),
        Af { inner } => lfp(&sat(inner, n, succ, labels), n, succ, true),
        Eg { inner } => gfp(&sat(inner, n, succ, labels), n, succ, false),
        Ag { inner } => gfp(&sat(inner, n, succ, labels), n, succ, true),
        Eu { left, right } => {
            let a = sat(left, n, succ, labels);
            let b = sat(right, n, succ, labels);
            until(&a, &b, n, succ, false)
        }
        Au { left, right } => {
            let a = sat(left, n, succ, labels);
            let b = sat(right, n, succ, labels);
            until(&a, &b, n, succ, true)
        }
    }
}

pub async fn tool_behavioral_check(
    _ctx: &SystemContext,
    params: BehavioralCheckParams,
) -> Result<CallToolResult, McpError> {
    let n = params.num_states;
    if params.initial >= n {
        return Err(McpError::invalid_params(
            format!("initial state {} out of range 0..{}", params.initial, n),
            None,
        ));
    }
    let mut succ: Vec<Vec<usize>> = vec![Vec::new(); n];
    for e in &params.transitions {
        if e.from < n && e.to < n {
            succ[e.from].push(e.to);
        }
    }
    let labels: Vec<HashSet<String>> = (0..n)
        .map(|i| {
            params
                .labels
                .get(i)
                .map(|v| v.iter().cloned().collect())
                .unwrap_or_default()
        })
        .collect();

    let satisfying = sat(&params.formula, n, &succ, &labels);
    let holds = satisfying.contains(&params.initial);
    let mut sat_states: Vec<usize> = satisfying.into_iter().collect();
    sat_states.sort_unstable();

    json_result(&json!({
        "holds": holds,
        "initial": params.initial,
        "satisfying_states": sat_states,
        "method": "CTL model-checking (Clarke–Emerson–Sistla fixpoint labelling)",
    }))
}
