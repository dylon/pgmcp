//! Chidamber-Kemerer object-oriented metrics (Chidamber & Kemerer, "A Metrics
//! Suite for Object Oriented Design", TSE 1994). (graph-roadmap Phase 4.3)
//!
//! The graph-traversal CK metrics — **DIT** (Depth of Inheritance Tree) and
//! **NOC** (Number Of Children) — are computed here from the inheritance edges
//! the symbol extractors already emit (`symbol_references.ref_kind IN
//! ('inherit','impl')`). The arithmetic CK metrics — **WMC** (Σ method
//! cyclomatic), **CBO** (coupling), **RFC** (methods + distinct calls) — are
//! aggregated in SQL by the tool from `function_metrics` / `symbol_references`.
//!
//! Pure + cycle-safe: the caller supplies the child→parents edge map; this
//! computes DIT (longest inheritance chain up to a root) and NOC (direct
//! children) per class.

use std::collections::{HashMap, HashSet};

/// DIT + NOC per class id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DitNoc {
    /// Depth of Inheritance Tree: longest chain of `inherit`/`impl` edges from
    /// this class up to a root (a class with no recorded parent). A root = 0.
    pub dit: u32,
    /// Number of Children: classes that directly inherit from this class.
    pub noc: u32,
}

/// Compute DIT + NOC for every class in `classes` from `child_to_parents`
/// (class id → its direct supertype class ids; parents not in `classes` are
/// still counted for depth). Inheritance cycles (illegal, but possible from
/// imprecise resolution) are broken by a visited set so DIT stays finite.
pub fn dit_noc(classes: &[i64], child_to_parents: &HashMap<i64, Vec<i64>>) -> HashMap<i64, DitNoc> {
    // NOC: each parent gets +1 per distinct child that names it.
    let mut noc: HashMap<i64, u32> = HashMap::new();
    for (child, parents) in child_to_parents {
        let mut seen: HashSet<i64> = HashSet::new();
        for &p in parents {
            if p != *child && seen.insert(p) {
                *noc.entry(p).or_insert(0) += 1;
            }
        }
    }

    // DIT: memoized longest path up, with on-stack cycle guard.
    let mut dit_cache: HashMap<i64, u32> = HashMap::new();
    fn depth(
        c: i64,
        c2p: &HashMap<i64, Vec<i64>>,
        cache: &mut HashMap<i64, u32>,
        stack: &mut HashSet<i64>,
    ) -> u32 {
        if let Some(&d) = cache.get(&c) {
            return d;
        }
        if !stack.insert(c) {
            return 0; // cycle: treat as root to terminate
        }
        let d = match c2p.get(&c) {
            Some(parents) if !parents.is_empty() => parents
                .iter()
                .filter(|&&p| p != c)
                .map(|&p| 1 + depth(p, c2p, cache, stack))
                .max()
                .unwrap_or(0),
            _ => 0,
        };
        stack.remove(&c);
        cache.insert(c, d);
        d
    }

    let mut out: HashMap<i64, DitNoc> = HashMap::with_capacity(classes.len());
    for &c in classes {
        let mut stack = HashSet::new();
        let dit = depth(c, child_to_parents, &mut dit_cache, &mut stack);
        out.insert(
            c,
            DitNoc {
                dit,
                noc: noc.get(&c).copied().unwrap_or(0),
            },
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_inheritance_chain_depth() {
        // 3 -> 2 -> 1 -> 0 (0 is the root). DIT(3)=3, NOC(0)=1.
        let mut c2p: HashMap<i64, Vec<i64>> = HashMap::new();
        c2p.insert(3, vec![2]);
        c2p.insert(2, vec![1]);
        c2p.insert(1, vec![0]);
        let m = dit_noc(&[0, 1, 2, 3], &c2p);
        assert_eq!(m[&0].dit, 0);
        assert_eq!(m[&3].dit, 3);
        assert_eq!(m[&1].dit, 1);
        assert_eq!(m[&0].noc, 1, "0 has one direct child (1)");
        assert_eq!(m[&3].noc, 0, "leaf has no children");
    }

    #[test]
    fn multiple_inheritance_takes_max_depth() {
        // 2 inherits from both 0 (root) and 1 (which inherits 0): DIT(2)=2.
        let mut c2p: HashMap<i64, Vec<i64>> = HashMap::new();
        c2p.insert(1, vec![0]);
        c2p.insert(2, vec![0, 1]);
        let m = dit_noc(&[0, 1, 2], &c2p);
        assert_eq!(m[&2].dit, 2, "max over parents (via 1) + 1");
        assert_eq!(m[&0].noc, 2, "0 has children 1 and 2");
    }

    #[test]
    fn cycle_is_finite() {
        // 0 -> 1 -> 0 (illegal cycle). The on-stack guard must make DIT
        // terminate and stay bounded by the number of classes (no infinite
        // recursion); the exact value within a cycle is unspecified.
        let mut c2p: HashMap<i64, Vec<i64>> = HashMap::new();
        c2p.insert(0, vec![1]);
        c2p.insert(1, vec![0]);
        let m = dit_noc(&[0, 1], &c2p);
        assert!(
            m[&0].dit <= 2 && m[&1].dit <= 2,
            "cycle must terminate with a bounded DIT, got {:?}",
            m
        );
    }
}
