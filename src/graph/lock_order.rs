//! Interprocedural lock-order graph + cycle detection (shadow-ASR concurrency).
//!
//! Pure algorithm (no DB): given each symbol's ordered lock events (acquire /
//! release / call, from the `sync_ops` skeleton) and a per-symbol summary of
//! the locks acquirable within K calls, build the lock-order graph — a directed
//! edge `A → B` for every "B acquired while A is held" — and report its cycles
//! (Tarjan SCC via [`crate::graph::algorithms::find_cycles`], witnessed by
//! [`extract_simple_cycles`]). A cycle is a Havender (1968) circular-wait
//! deadlock candidate; soundness (`acyclic ⇒ deadlock-free`) is proved in
//! `docs/formal/rocq/LockOrderDeadlock.v`.
//!
//! The interprocedural edges are the RacerD/Infer "deadlock domain" idea: at a
//! call site reached while holding `A`, every lock the callee can acquire
//! (within K hops) is ordered after `A`. This is the precision the old
//! regex-only `deadlock_candidates` lacks (it never crossed a function
//! boundary).

use std::collections::HashMap;

use petgraph::graph::{DiGraph, NodeIndex};

use crate::graph::algorithms::{extract_simple_cycles, find_cycles};

/// Read vs. write acquisition — drives the rwlock refinement (an all-read cycle
/// cannot deadlock).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AcqMode {
    Read,
    Write,
}

/// One lock-relevant event in a symbol's body, in program (line/seq) order.
#[derive(Clone, Debug)]
pub enum LockEvent {
    Acquire {
        key: String,
        mode: AcqMode,
        conf: f32,
        line: u32,
    },
    Release {
        key: String,
    },
    /// A resolved outgoing call — at this point the callee may acquire any lock
    /// in its reachable-acquire summary (interprocedural inlining).
    Call {
        callee: i64,
    },
}

/// A representative acquisition of a lock, used to witness interprocedural
/// edges (where the acquire happens inside a callee subtree).
#[derive(Clone, Debug)]
pub struct ReachAcq {
    pub mode: AcqMode,
    pub conf: f32,
    pub symbol_id: i64,
    pub line: u32,
}

/// A directed lock-order edge `from → to` ("`to` acquired while `from` held").
#[derive(Clone, Debug)]
pub struct LockEdge {
    pub from: String,
    pub to: String,
    pub from_mode: AcqMode,
    pub to_mode: AcqMode,
    pub min_confidence: f32,
    pub interprocedural: bool,
    /// Symbol + line where `from` is held.
    pub held_symbol: i64,
    pub held_line: u32,
    /// Symbol + line where `to` is acquired (the callee side, for interproc).
    pub acquired_symbol: i64,
    pub acquired_line: u32,
    /// The immediate callee through which `to` becomes reachable (interproc).
    pub via_callee: Option<i64>,
}

/// A detected lock-order cycle: the participating resources (normalized order)
/// plus one representative edge per consecutive pair (closing the loop).
#[derive(Clone, Debug)]
pub struct LockCycle {
    pub resources: Vec<String>,
    pub edges: Vec<LockEdge>,
}

impl LockCycle {
    /// True iff every edge is read-held→read-acquired — shared reads do not
    /// deadlock under a standard rwlock, so such a cycle is informational.
    pub fn is_all_read(&self) -> bool {
        !self.edges.is_empty()
            && self
                .edges
                .iter()
                .all(|e| e.from_mode == AcqMode::Read && e.to_mode == AcqMode::Read)
    }

    /// The weakest resource-identity confidence along the cycle (its strength).
    pub fn min_confidence(&self) -> f32 {
        self.edges
            .iter()
            .map(|e| e.min_confidence)
            .fold(f32::INFINITY, f32::min)
    }
}

/// One held lock on the per-symbol acquisition stack.
#[derive(Clone)]
struct Held {
    key: String,
    mode: AcqMode,
    conf: f32,
    line: u32,
}

/// Build the lock-order edges. `events_by_symbol` is each symbol's ordered lock
/// events; `reachable_acq[callee]` is the locks acquirable within K hops from a
/// callee (keyed by resource). Edges below `confidence_floor` are dropped.
pub fn build_lock_order(
    events_by_symbol: &HashMap<i64, Vec<LockEvent>>,
    reachable_acq: &HashMap<i64, HashMap<String, ReachAcq>>,
    confidence_floor: f32,
) -> Vec<LockEdge> {
    let mut edges: Vec<LockEdge> = Vec::new();

    for (&sym, events) in events_by_symbol {
        let mut held: Vec<Held> = Vec::new();
        for ev in events {
            match ev {
                LockEvent::Acquire {
                    key,
                    mode,
                    conf,
                    line,
                } => {
                    if *conf < confidence_floor || key.is_empty() {
                        continue;
                    }
                    for h in &held {
                        if h.key != *key {
                            edges.push(LockEdge {
                                from: h.key.clone(),
                                to: key.clone(),
                                from_mode: h.mode,
                                to_mode: *mode,
                                min_confidence: h.conf.min(*conf),
                                interprocedural: false,
                                held_symbol: sym,
                                held_line: h.line,
                                acquired_symbol: sym,
                                acquired_line: *line,
                                via_callee: None,
                            });
                        }
                    }
                    held.push(Held {
                        key: key.clone(),
                        mode: *mode,
                        conf: *conf,
                        line: *line,
                    });
                }
                LockEvent::Release { key } => {
                    if let Some(pos) = held.iter().rposition(|h| h.key == *key) {
                        held.remove(pos);
                    }
                }
                LockEvent::Call { callee } => {
                    if held.is_empty() {
                        continue;
                    }
                    let Some(reach) = reachable_acq.get(callee) else {
                        continue;
                    };
                    for (bkey, racq) in reach {
                        if racq.conf < confidence_floor {
                            continue;
                        }
                        for h in &held {
                            if h.key != *bkey {
                                edges.push(LockEdge {
                                    from: h.key.clone(),
                                    to: bkey.clone(),
                                    from_mode: h.mode,
                                    to_mode: racq.mode,
                                    min_confidence: h.conf.min(racq.conf),
                                    interprocedural: true,
                                    held_symbol: sym,
                                    held_line: h.line,
                                    acquired_symbol: racq.symbol_id,
                                    acquired_line: racq.line,
                                    via_callee: Some(*callee),
                                });
                            }
                        }
                    }
                }
            }
        }
    }
    edges
}

/// Detect cycles in the lock-order graph and witness each with representative
/// edges. `max_cycle_len` bounds simple-cycle enumeration.
pub fn find_lock_cycles(edges: &[LockEdge], max_cycle_len: usize) -> Vec<LockCycle> {
    if edges.is_empty() {
        return Vec::new();
    }
    let mut idx: HashMap<String, NodeIndex> = HashMap::new();
    let mut g: DiGraph<String, ()> = DiGraph::new();
    // For witness reconstruction: best (highest-confidence) edge per (from,to).
    let mut best_edge: HashMap<(String, String), usize> = HashMap::new();

    for (i, e) in edges.iter().enumerate() {
        let a = *idx
            .entry(e.from.clone())
            .or_insert_with(|| g.add_node(e.from.clone()));
        let b = *idx
            .entry(e.to.clone())
            .or_insert_with(|| g.add_node(e.to.clone()));
        g.add_edge(a, b, ());
        let pair = (e.from.clone(), e.to.clone());
        match best_edge.get(&pair) {
            Some(&prev) if edges[prev].min_confidence >= e.min_confidence => {}
            _ => {
                best_edge.insert(pair, i);
            }
        }
    }

    let mut out: Vec<LockCycle> = Vec::new();
    let mut seen: std::collections::HashSet<Vec<String>> = std::collections::HashSet::new();
    for scc in find_cycles(&g) {
        for cyc in extract_simple_cycles(&g, &scc, max_cycle_len) {
            let keys: Vec<String> = cyc.iter().map(|n| g[*n].clone()).collect();
            if keys.len() < 2 {
                continue;
            }
            // Dedup by the normalized resource set+order.
            if !seen.insert(keys.clone()) {
                continue;
            }
            let mut cyc_edges = Vec::with_capacity(keys.len());
            let mut complete = true;
            for w in 0..keys.len() {
                let from = &keys[w];
                let to = &keys[(w + 1) % keys.len()];
                match best_edge.get(&(from.clone(), to.clone())) {
                    Some(&ei) => cyc_edges.push(edges[ei].clone()),
                    None => {
                        complete = false;
                        break;
                    }
                }
            }
            if complete {
                out.push(LockCycle {
                    resources: keys,
                    edges: cyc_edges,
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acq(key: &str, mode: AcqMode, line: u32) -> LockEvent {
        LockEvent::Acquire {
            key: key.into(),
            mode,
            conf: 0.9,
            line,
        }
    }
    fn rel(key: &str) -> LockEvent {
        LockEvent::Release { key: key.into() }
    }

    #[test]
    fn intraprocedural_ab_ba_cycle() {
        // f: lock A; lock B   |   g: lock B; lock A   → cycle {A,B}.
        let mut ev = HashMap::new();
        ev.insert(
            1,
            vec![acq("A", AcqMode::Write, 1), acq("B", AcqMode::Write, 2)],
        );
        ev.insert(
            2,
            vec![acq("B", AcqMode::Write, 1), acq("A", AcqMode::Write, 2)],
        );
        let edges = build_lock_order(&ev, &HashMap::new(), 0.3);
        let cycles = find_lock_cycles(&edges, 6);
        assert_eq!(cycles.len(), 1, "one A/B cycle, got {cycles:?}");
        let mut res = cycles[0].resources.clone();
        res.sort();
        assert_eq!(res, vec!["A".to_string(), "B".to_string()]);
        assert!(!cycles[0].is_all_read());
    }

    #[test]
    fn correct_lock_order_has_no_cycle() {
        // Both functions acquire A then B → acyclic.
        let mut ev = HashMap::new();
        ev.insert(
            1,
            vec![
                acq("A", AcqMode::Write, 1),
                acq("B", AcqMode::Write, 2),
                rel("B"),
                rel("A"),
            ],
        );
        ev.insert(
            2,
            vec![
                acq("A", AcqMode::Write, 1),
                acq("B", AcqMode::Write, 2),
                rel("B"),
                rel("A"),
            ],
        );
        let edges = build_lock_order(&ev, &HashMap::new(), 0.3);
        assert!(find_lock_cycles(&edges, 6).is_empty());
    }

    #[test]
    fn reentrant_same_lock_no_self_edge() {
        let mut ev = HashMap::new();
        ev.insert(
            1,
            vec![acq("A", AcqMode::Write, 1), acq("A", AcqMode::Write, 2)],
        );
        let edges = build_lock_order(&ev, &HashMap::new(), 0.3);
        assert!(edges.is_empty(), "no self A→A edge for re-entrant lock");
    }

    #[test]
    fn interprocedural_cycle_via_callee() {
        // f(sym1): hold A, then call g(sym2). g reaches acquire of B.
        // h(sym3): hold B, then call k(sym4). k reaches acquire of A.
        // → A→B (via g) and B→A (via k) ⇒ cycle.
        let mut ev = HashMap::new();
        ev.insert(
            1,
            vec![acq("A", AcqMode::Write, 1), LockEvent::Call { callee: 2 }],
        );
        ev.insert(
            3,
            vec![acq("B", AcqMode::Write, 1), LockEvent::Call { callee: 4 }],
        );
        let mut reach = HashMap::new();
        let mut g_reach = HashMap::new();
        g_reach.insert(
            "B".to_string(),
            ReachAcq {
                mode: AcqMode::Write,
                conf: 0.8,
                symbol_id: 20,
                line: 9,
            },
        );
        reach.insert(2i64, g_reach);
        let mut k_reach = HashMap::new();
        k_reach.insert(
            "A".to_string(),
            ReachAcq {
                mode: AcqMode::Write,
                conf: 0.8,
                symbol_id: 40,
                line: 9,
            },
        );
        reach.insert(4i64, k_reach);
        let edges = build_lock_order(&ev, &reach, 0.3);
        let cycles = find_lock_cycles(&edges, 6);
        assert_eq!(cycles.len(), 1, "interprocedural A/B cycle: {edges:?}");
        assert!(cycles[0].edges.iter().any(|e| e.interprocedural));
    }

    #[test]
    fn all_read_cycle_flagged() {
        // Two read-locks acquired in opposite order — a cycle, but all-read.
        let mut ev = HashMap::new();
        ev.insert(
            1,
            vec![acq("A", AcqMode::Read, 1), acq("B", AcqMode::Read, 2)],
        );
        ev.insert(
            2,
            vec![acq("B", AcqMode::Read, 1), acq("A", AcqMode::Read, 2)],
        );
        let edges = build_lock_order(&ev, &HashMap::new(), 0.3);
        let cycles = find_lock_cycles(&edges, 6);
        assert_eq!(cycles.len(), 1);
        assert!(cycles[0].is_all_read(), "RR cycle must be flagged all-read");
    }

    #[test]
    fn confidence_floor_drops_weak_edges() {
        let mut ev = HashMap::new();
        ev.insert(
            1,
            vec![
                LockEvent::Acquire {
                    key: "A".into(),
                    mode: AcqMode::Write,
                    conf: 0.2,
                    line: 1,
                },
                LockEvent::Acquire {
                    key: "B".into(),
                    mode: AcqMode::Write,
                    conf: 0.2,
                    line: 2,
                },
            ],
        );
        let edges = build_lock_order(&ev, &HashMap::new(), 0.5);
        assert!(
            edges.is_empty(),
            "0.2-confidence acquires are below the 0.5 floor"
        );
    }
}
