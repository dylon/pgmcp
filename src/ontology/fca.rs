//! Formal Concept Analysis — the deterministic `is_a` backbone.
//!
//! Each concept is described by an **attribute set** (the features its anchored
//! code units exhibit — shadow-ASR effects in Phase 4). Ordering concepts by
//! attribute-set inclusion yields a strict partial order whose **Hasse cover**
//! is the `is_a` relation: a concept with a strict *superset* of attributes is a
//! more specific kind of the one with the subset, and we keep only the immediate
//! (non-transitive) covers. The order is acyclic by construction (strict subset
//! ⇒ strictly increasing cardinality), so the emitted edge set is a DAG.
//!
//! This is the explainable, hallucination-free complement to clustering + LLM
//! naming: every `is_a` edge is justified by a concrete attribute-superset
//! relationship over real code features. Pure + exhaustively testable.

use std::collections::BTreeSet;

/// A concept tagged with its attribute set (interned attribute ids).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConceptAttrs {
    pub entity_id: i64,
    pub attrs: BTreeSet<u32>,
}

impl ConceptAttrs {
    /// Convenience constructor (tests + callers that build attribute sets
    /// in-memory; the cron path constructs the struct fields directly).
    #[allow(dead_code)]
    pub fn new(entity_id: i64, attrs: impl IntoIterator<Item = u32>) -> Self {
        Self {
            entity_id,
            attrs: attrs.into_iter().collect(),
        }
    }
}

/// Compute the `is_a` Hasse cover: returns `(child, parent)` pairs where
/// `parent.attrs ⊊ child.attrs` (child strictly more specific) and there is **no
/// intermediate** concept `k` with `parent.attrs ⊊ k.attrs ⊊ child.attrs`.
///
/// Concepts with equal attribute sets are *not* `is_a`-related (they sit at the
/// same specificity). The result is acyclic (a transitive reduction of a strict
/// partial order). O(n²) candidate pairs × O(n) intermediate check — bounded by
/// the (small) per-facet concept count.
pub fn is_a_cover(concepts: &[ConceptAttrs]) -> Vec<(i64, i64)> {
    let n = concepts.len();
    // Preallocate generously: covers are sparse, but avoid early reallocs.
    let mut edges: Vec<(i64, i64)> = Vec::with_capacity(n);
    for i in 0..n {
        for j in 0..n {
            if i == j {
                continue;
            }
            // i `is_a` j  ⇔  attrs(j) ⊊ attrs(i)  (i more specific than j).
            let i_more_specific = concepts[j].attrs.len() < concepts[i].attrs.len()
                && concepts[j].attrs.is_subset(&concepts[i].attrs);
            if !i_more_specific {
                continue;
            }
            // Keep only the immediate cover: drop (i,j) if some k sits strictly
            // between j and i in the attribute-inclusion order.
            let has_intermediate = (0..n).any(|k| {
                k != i
                    && k != j
                    && concepts[k].attrs.len() > concepts[j].attrs.len()
                    && concepts[k].attrs.len() < concepts[i].attrs.len()
                    && concepts[j].attrs.is_subset(&concepts[k].attrs)
                    && concepts[k].attrs.is_subset(&concepts[i].attrs)
            });
            if !has_intermediate {
                edges.push((concepts[i].entity_id, concepts[j].entity_id));
            }
        }
    }
    edges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cover_is_transitive_reduction() {
        // R{} ⊂ M{1} ⊂ {S1{1,2}, S2{1,3}}.
        let concepts = vec![
            ConceptAttrs::new(10, []),     // R — most general
            ConceptAttrs::new(20, [1]),    // M
            ConceptAttrs::new(30, [1, 2]), // S1
            ConceptAttrs::new(40, [1, 3]), // S2
        ];
        let mut edges = is_a_cover(&concepts);
        edges.sort_unstable();
        // M is_a R; S1 is_a M; S2 is_a M. NOT S1/S2 is_a R (M is intermediate).
        assert_eq!(edges, vec![(20, 10), (30, 20), (40, 20)]);
    }

    #[test]
    fn equal_attribute_sets_are_not_is_a() {
        let concepts = vec![ConceptAttrs::new(1, [1, 2]), ConceptAttrs::new(2, [1, 2])];
        assert!(is_a_cover(&concepts).is_empty());
    }

    #[test]
    fn diamond_keeps_both_paths_but_no_shortcut() {
        // bottom{1,2,3} ⊃ {A{1,2}, B{1,3}} ⊃ top{1}
        let concepts = vec![
            ConceptAttrs::new(1, [1]),       // top
            ConceptAttrs::new(2, [1, 2]),    // A
            ConceptAttrs::new(3, [1, 3]),    // B
            ConceptAttrs::new(4, [1, 2, 3]), // bottom
        ];
        let mut edges = is_a_cover(&concepts);
        edges.sort_unstable();
        // bottom is_a A, bottom is_a B, A is_a top, B is_a top. No bottom is_a top.
        assert_eq!(edges, vec![(2, 1), (3, 1), (4, 2), (4, 3)]);
        assert!(
            !edges.contains(&(4, 1)),
            "transitive shortcut must be removed"
        );
    }

    #[test]
    fn result_is_acyclic() {
        let concepts = vec![
            ConceptAttrs::new(1, [1]),
            ConceptAttrs::new(2, [1, 2]),
            ConceptAttrs::new(3, [1, 2, 3]),
        ];
        let edges = is_a_cover(&concepts);
        // Every edge points strictly "up" (child has more attrs than parent),
        // so no cycle can exist.
        for (child, parent) in &edges {
            let c = concepts.iter().find(|x| x.entity_id == *child).unwrap();
            let p = concepts.iter().find(|x| x.entity_id == *parent).unwrap();
            assert!(p.attrs.len() < c.attrs.len());
        }
    }
}
