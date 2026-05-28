//! Phase 8 — passive FSM inference over observed coordination traces (ADR-009).
//!
//! Active L\* (Angluin) needs a live membership + equivalence oracle, which a
//! nondeterministic LLM peer cannot provide; so we infer a **passive** model
//! from recorded traces — a prefix-tree acceptor (PTA) with per-edge observation
//! counts (a Mealy/Markov view: each `(state, symbol)` carries how often it was
//! seen). The PTA is then diffed against the declared protocol: symbols observed
//! that the protocol's alphabet does not contain are *novel* (a drifted or
//! misconfigured peer), and the conformant fraction of the source runs measures
//! how well the peer's behaviour matches the spec. This is the AgentGuard-style
//! "learn the peer's behaviour, check it against the model" posture, honest about
//! being passive + frequency-based rather than exact active learning.

use std::collections::{BTreeMap, BTreeSet};

/// A symbol in an observed trace — here a communication rendered `from->to:label`.
pub type Symbol = String;

/// An inferred deterministic prefix-tree automaton with observation counts.
#[derive(Debug, Clone)]
pub struct InferredFsm {
    pub n_states: usize,
    pub initial: usize,
    /// `(from_state, symbol) -> (to_state, observation_count)`.
    pub edges: BTreeMap<(usize, Symbol), (usize, u64)>,
    /// States at which some trace ended.
    pub accepting: BTreeSet<usize>,
    pub n_traces: u64,
}

impl InferredFsm {
    /// Distinct symbols (the inferred alphabet).
    pub fn alphabet(&self) -> BTreeSet<Symbol> {
        self.edges.keys().map(|(_, s)| s.clone()).collect()
    }

    /// Observed symbols absent from `allowed` (the declared protocol's alphabet)
    /// — the peer did something the protocol never prescribes.
    pub fn novel_symbols(&self, allowed: &BTreeSet<Symbol>) -> Vec<Symbol> {
        self.alphabet()
            .into_iter()
            .filter(|s| !allowed.contains(s))
            .collect()
    }

    /// A JSON-friendly edge listing: `[{from, symbol, to, count}]`.
    pub fn edges_json(&self) -> Vec<serde_json::Value> {
        self.edges
            .iter()
            .map(|((from, sym), (to, count))| {
                serde_json::json!({ "from": from, "symbol": sym, "to": to, "count": count })
            })
            .collect()
    }
}

/// Infer the canonical prefix-tree acceptor (with observation counts) from a set
/// of symbol-sequence traces. Deterministic: a shared prefix maps to a shared
/// state; counts accumulate how often each transition was observed.
pub fn infer_prefix_tree(traces: &[Vec<Symbol>]) -> InferredFsm {
    let mut edges: BTreeMap<(usize, Symbol), (usize, u64)> = BTreeMap::new();
    let mut accepting: BTreeSet<usize> = BTreeSet::new();
    let mut n_states = 1; // state 0 = root (empty prefix)
    for trace in traces {
        let mut state = 0usize;
        for sym in trace {
            let key = (state, sym.clone());
            if let Some((to, count)) = edges.get_mut(&key) {
                *count += 1;
                state = *to;
            } else {
                let to = n_states;
                n_states += 1;
                edges.insert(key, (to, 1));
                state = to;
            }
        }
        accepting.insert(state);
    }
    InferredFsm {
        n_states,
        initial: 0,
        edges,
        accepting,
        n_traces: traces.len() as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(syms: &[&str]) -> Vec<Symbol> {
        syms.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn prefix_tree_shares_common_prefixes_and_counts() {
        // Two traces sharing the prefix [a, b]; one extends with c, the other d.
        let traces = [t(&["a", "b", "c"]), t(&["a", "b", "d"])];
        let fsm = infer_prefix_tree(&traces);
        assert_eq!(fsm.n_traces, 2);
        // Shared prefix a,b => the a-edge and b-edge are each observed twice.
        let a = fsm.edges.get(&(0, "a".to_string())).expect("a edge");
        assert_eq!(a.1, 2, "the shared 'a' transition is observed twice");
        let b = fsm.edges.get(&(a.0, "b".to_string())).expect("b edge");
        assert_eq!(b.1, 2);
        // Then it branches into c and d (count 1 each) → two accepting leaves.
        assert_eq!(fsm.accepting.len(), 2);
        assert_eq!(
            fsm.alphabet(),
            BTreeSet::from(["a".into(), "b".into(), "c".into(), "d".into()])
        );
    }

    #[test]
    fn novel_symbols_flag_off_protocol_behaviour() {
        let fsm = infer_prefix_tree(&[t(&["O->P:plan_req", "P->O:plan", "P->O:rogue"])]);
        let allowed: BTreeSet<Symbol> = ["O->P:plan_req".into(), "P->O:plan".into()]
            .into_iter()
            .collect();
        let novel = fsm.novel_symbols(&allowed);
        assert_eq!(novel, vec!["P->O:rogue".to_string()]);
    }

    #[test]
    fn empty_corpus_is_just_the_root() {
        let fsm = infer_prefix_tree(&[]);
        assert_eq!(fsm.n_states, 1);
        assert!(fsm.edges.is_empty());
        assert!(fsm.accepting.is_empty());
    }
}
