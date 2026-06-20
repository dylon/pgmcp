//! **Formal Concept Analysis** (FCA) over a real incidence relation
//! (ADR-028, CT-4).
//!
//! A *formal context* is a triple `(G, M, I)`: a set of objects `G`, a set of
//! attributes `M`, and an incidence relation `I ⊆ G × M` (`g I m` ≡ "object `g`
//! has attribute `m`"). The two **derivation operators** form a Galois
//! connection:
//!
//! ```text
//!   A' = { m ∈ M | ∀ g ∈ A. g I m }     (A ⊆ G)   — attributes shared by all of A
//!   B' = { g ∈ G | ∀ m ∈ B. g I m }     (B ⊆ M)   — objects sharing all of B
//! ```
//!
//! A **formal concept** is a pair `(A, B)` with `A' = B` and `B' = A`; `A` is the
//! *extent*, `B` the *intent*. Intents are exactly the **closed** attribute sets
//! (`B'' = B`); extents the closed object sets. Concepts ordered by extent
//! inclusion form a complete lattice — the **concept lattice** `𝔅(G, M, I)`.
//!
//! This module is the pure, database-free core: it builds a [`FormalContext`]
//! from index vectors, exposes the [`FormalContext::attr_closure`] /
//! [`FormalContext::extent_of`] / [`FormalContext::intent_of`] operators, and
//! enumerates **all** concepts with **Ganter's NextClosure** algorithm in lectic
//! order (Ganter, B. 1984, "Two basic algorithms in concept analysis";
//! Ganter & Wille 1999, *Formal Concept Analysis: Mathematical Foundations*,
//! Springer, doi:10.1007/978-3-642-59830-2). It also derives the Hasse covering
//! relation and simple extent-drop attribute implications.
//!
//! The attribute set is small for the two grounded contexts (effects ≈ a dozen,
//! type-tags ≈ tens), so NextClosure — `O(|M|² · |G| · #concepts)` — is
//! tractable. The MCP tool bounds the enumeration with `max_concepts` and logs a
//! truncation via `error!` (ADR-021/022: no silent caps).

use std::collections::BTreeMap;

use serde::Serialize;

/// A fixed-width bitset over `0..n` backed by `u64` words. Kept private and
/// minimal (membership, union/intersection-in-place, iteration, equality) — just
/// enough for the derivation operators. Avoids a new crate dependency.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BitSet {
    words: Vec<u64>,
    nbits: usize,
}

impl BitSet {
    /// An all-zero set over `0..nbits`.
    pub fn empty(nbits: usize) -> Self {
        BitSet {
            words: vec![0; nbits.div_ceil(64)],
            nbits,
        }
    }

    /// The full set `{0, …, nbits-1}`.
    pub fn full(nbits: usize) -> Self {
        let mut s = BitSet::empty(nbits);
        for i in 0..nbits {
            s.insert(i);
        }
        s
    }

    #[inline]
    pub fn insert(&mut self, i: usize) {
        debug_assert!(i < self.nbits);
        self.words[i / 64] |= 1u64 << (i % 64);
    }

    #[inline]
    pub fn contains(&self, i: usize) -> bool {
        i < self.nbits && (self.words[i / 64] >> (i % 64)) & 1 == 1
    }

    /// In-place intersection: `self ∩= other`. Panics if widths differ.
    pub fn intersect_with(&mut self, other: &BitSet) {
        debug_assert_eq!(self.nbits, other.nbits);
        for (a, b) in self.words.iter_mut().zip(&other.words) {
            *a &= *b;
        }
    }

    /// Number of set bits.
    pub fn count(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// `self ⊆ other`?
    pub fn is_subset(&self, other: &BitSet) -> bool {
        debug_assert_eq!(self.nbits, other.nbits);
        self.words
            .iter()
            .zip(&other.words)
            .all(|(a, b)| a & b == *a)
    }

    /// Iterate set indices in ascending order.
    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.nbits).filter(move |&i| self.contains(i))
    }

    pub fn nbits(&self) -> usize {
        self.nbits
    }
}

/// A formal context `(G, M, I)` with objects and attributes carried by external
/// label vectors. Incidence is stored twice for fast derivation in both
/// directions: `obj_attrs[g]` = the attribute bitset of object `g` (rows),
/// `attr_objs[m]` = the object bitset of attribute `m` (columns).
#[derive(Debug, Clone)]
pub struct FormalContext {
    pub objects: Vec<String>,
    pub attributes: Vec<String>,
    obj_attrs: Vec<BitSet>,
    attr_objs: Vec<BitSet>,
}

/// One formal concept `(extent, intent)` with both sides as bitsets over the
/// object / attribute index spaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Concept {
    pub extent: BitSet,
    pub intent: BitSet,
}

/// An attribute implication `premise ⟹ conclusion` with its support (the number
/// of objects whose attribute set contains the premise — i.e. `|premise'|`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Implication {
    pub premise: Vec<String>,
    pub conclusion: Vec<String>,
    pub support: usize,
}

impl FormalContext {
    /// Build a context from `objects`, `attributes`, and an incidence list of
    /// `(object_index, attribute_index)` pairs. Out-of-range pairs are ignored
    /// (defensive; the SQL upstream never produces them).
    pub fn new(
        objects: Vec<String>,
        attributes: Vec<String>,
        incidence: impl IntoIterator<Item = (usize, usize)>,
    ) -> Self {
        let n_obj = objects.len();
        let n_attr = attributes.len();
        let mut obj_attrs = vec![BitSet::empty(n_attr); n_obj];
        let mut attr_objs = vec![BitSet::empty(n_obj); n_attr];
        for (g, m) in incidence {
            if g < n_obj && m < n_attr {
                obj_attrs[g].insert(m);
                attr_objs[m].insert(g);
            }
        }
        FormalContext {
            objects,
            attributes,
            obj_attrs,
            attr_objs,
        }
    }

    pub fn n_objects(&self) -> usize {
        self.objects.len()
    }
    pub fn n_attributes(&self) -> usize {
        self.attributes.len()
    }

    /// The `B ↦ B'` operator: objects sharing **all** attributes in `intent`
    /// (the intersection of the attribute columns). The empty intent derives all
    /// objects (the universal extent).
    pub fn extent_of(&self, intent: &BitSet) -> BitSet {
        let mut acc = BitSet::full(self.n_objects());
        for m in intent.iter() {
            acc.intersect_with(&self.attr_objs[m]);
        }
        acc
    }

    /// The `A ↦ A'` operator: attributes common to **all** objects in `extent`
    /// (the intersection of the object rows). The empty extent derives all
    /// attributes.
    pub fn intent_of(&self, extent: &BitSet) -> BitSet {
        let mut acc = BitSet::full(self.n_attributes());
        for g in extent.iter() {
            acc.intersect_with(&self.obj_attrs[g]);
        }
        acc
    }

    /// The attribute closure `B ↦ B'' `: the intent of `B`'s extent. A set is
    /// **closed** iff `attr_closure(B) == B`; closed attribute sets are exactly
    /// the concept intents.
    pub fn attr_closure(&self, intent: &BitSet) -> BitSet {
        let extent = self.extent_of(intent);
        self.intent_of(&extent)
    }

    /// Enumerate **all** formal concepts via **Ganter's NextClosure**, ascending
    /// in lectic order, starting from the closure of ∅ (the top concept by
    /// intent — all objects). Stops after `max_concepts` and sets the returned
    /// `truncated` flag; the caller logs the cap (ADR-022: no silent caps).
    ///
    /// NextClosure (Ganter 1984): given a closed set `B`, the lectically next
    /// closed set is found by, for `i` from the largest attribute down to the
    /// smallest, computing `C = closure((B ∩ {0..i}) ∪ {i})`; the first `i ∉ B`
    /// for which `C` adds nothing below `i` (`C \ B` has min element `i`) yields
    /// the next closed set `C`. Termination is when the candidate would be the
    /// full attribute set already enumerated.
    pub fn concepts(&self, max_concepts: usize) -> (Vec<Concept>, bool) {
        let n = self.n_attributes();
        let mut out: Vec<Concept> = Vec::new();

        // Start: closure of the empty intent (= attributes shared by every
        // object; for a clarified context this is ∅).
        let mut current = self.attr_closure(&BitSet::empty(n));
        let push = |out: &mut Vec<Concept>, ctx: &FormalContext, intent: BitSet| {
            let extent = ctx.extent_of(&intent);
            out.push(Concept { extent, intent });
        };
        push(&mut out, self, current.clone());
        if out.len() >= max_concepts {
            return (out, true);
        }

        // The full set is the final concept; iterate until we reach it.
        let full = BitSet::full(n);
        while current != full {
            match self.next_closure(&current) {
                Some(next) => {
                    current = next;
                    push(&mut out, self, current.clone());
                    if out.len() >= max_concepts && current != full {
                        return (out, true);
                    }
                }
                None => break,
            }
        }
        (out, false)
    }

    /// One NextClosure step: the lectically smallest closed set strictly greater
    /// than `b`, or `None` if `b` is the maximum (the full attribute set's
    /// closure). See [`FormalContext::concepts`] for the algorithm statement.
    fn next_closure(&self, b: &BitSet) -> Option<BitSet> {
        let n = self.n_attributes();
        // Walk i from n-1 down to 0.
        for i in (0..n).rev() {
            if b.contains(i) {
                continue; // `b ⊕ i` only makes sense for i ∉ b
            }
            // candidate seed = (b ∩ {0..i}) ∪ {i}
            let mut seed = BitSet::empty(n);
            for j in 0..i {
                if b.contains(j) {
                    seed.insert(j);
                }
            }
            seed.insert(i);
            let c = self.attr_closure(&seed);
            // Accept iff `c` adds nothing strictly below i (lectic "< i" test):
            // every element of c below i must already be in b.
            let ok_below = (0..i).all(|j| !c.contains(j) || b.contains(j));
            if ok_below {
                return Some(c);
            }
        }
        None
    }

    /// The **Hasse covering** relation of the concept lattice on extent
    /// inclusion: an edge `(child, parent)` means `child.extent ⊊ parent.extent`
    /// with no concept strictly between. Indices are into the `concepts` slice.
    /// `O(c² )` subset checks plus an `O(c)` "no intermediate" filter per pair;
    /// fine for the small concept counts here.
    pub fn covers(concepts: &[Concept]) -> Vec<(usize, usize)> {
        let c = concepts.len();
        // Direct strict-superset pairs by extent.
        let mut parents: Vec<Vec<usize>> = vec![Vec::new(); c];
        for (i, ci) in concepts.iter().enumerate() {
            for (j, cj) in concepts.iter().enumerate() {
                if i != j
                    && ci.extent.is_subset(&cj.extent)
                    && ci.extent.count() < cj.extent.count()
                {
                    parents[i].push(j); // j is an (not necessarily immediate) ancestor of i
                }
            }
        }
        let mut edges: Vec<(usize, usize)> = Vec::new();
        for (i, anc) in parents.iter().enumerate() {
            for &p in anc {
                // p covers i iff no k in anc(i) sits strictly between i and p:
                // i.extent ⊊ k.extent ⊊ p.extent.
                let intermediate = anc.iter().any(|&k| {
                    k != p
                        && concepts[i].extent.count() < concepts[k].extent.count()
                        && concepts[k].extent.count() < concepts[p].extent.count()
                        && concepts[i].extent.is_subset(&concepts[k].extent)
                        && concepts[k].extent.is_subset(&concepts[p].extent)
                });
                if !intermediate {
                    edges.push((i, p));
                }
            }
        }
        edges.sort_unstable();
        edges
    }

    /// Derive **attribute implications** from the cover relation: for each Hasse
    /// edge `child ⋖ parent`, the attributes the child's intent gains over the
    /// parent's are implied by the parent's intent (adding the parent intent
    /// forces the child's extra attributes — an extent-drop implication
    /// `parent_intent ⟹ gained`). Support is the parent extent size
    /// (`|premise'|`). Deduplicated by `(premise, conclusion)`.
    pub fn implications(
        &self,
        concepts: &[Concept],
        covers: &[(usize, usize)],
    ) -> Vec<Implication> {
        let mut seen: BTreeMap<(Vec<usize>, Vec<usize>), usize> = BTreeMap::new();
        for &(child, parent) in covers {
            let pi = &concepts[parent].intent;
            let ci = &concepts[child].intent;
            let premise: Vec<usize> = pi.iter().collect();
            let gained: Vec<usize> = ci.iter().filter(|m| !pi.contains(*m)).collect();
            if gained.is_empty() || premise.is_empty() {
                continue;
            }
            let support = concepts[parent].extent.count();
            seen.entry((premise, gained)).or_insert(support);
        }
        let mut out: Vec<Implication> = seen
            .into_iter()
            .map(|((premise, conclusion), support)| Implication {
                premise: premise
                    .iter()
                    .map(|&m| self.attributes[m].clone())
                    .collect(),
                conclusion: conclusion
                    .iter()
                    .map(|&m| self.attributes[m].clone())
                    .collect(),
                support,
            })
            .collect();
        // Stable, useful ordering: widest support first, then by premise.
        out.sort_by(|a, b| {
            b.support
                .cmp(&a.support)
                .then_with(|| a.premise.cmp(&b.premise))
                .then_with(|| a.conclusion.cmp(&b.conclusion))
        });
        out
    }

    /// Label a bitset of attribute indices.
    pub fn label_attrs(&self, b: &BitSet) -> Vec<String> {
        b.iter().map(|m| self.attributes[m].clone()).collect()
    }

    /// Label up to `cap` object indices of an extent (a sample, for display).
    pub fn sample_objects(&self, extent: &BitSet, cap: usize) -> Vec<String> {
        extent
            .iter()
            .take(cap)
            .map(|g| self.objects[g].clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the canonical 3×3 context used across the tests:
    ///
    /// ```text
    ///        a   b   c
    ///   o1   ×   ×
    ///   o2   ×       ×
    ///   o3   ×   ×   ×
    /// ```
    ///
    /// Hand-derived concept lattice (intents):
    ///   ⊤ = ({o1,o2,o3}, {a})            — a is shared by all
    ///       ({o1,o3},     {a,b})
    ///       ({o2,o3},     {a,c})
    ///   ⊥ = ({o3},        {a,b,c})
    /// (∅ extent with intent {a,b,c}'... here {a,b,c}'={o3}≠∅, so the bottom is
    /// {o3}; there is no separate empty-extent concept because every attribute
    /// pair already has o3.)
    fn ctx_3x3() -> FormalContext {
        FormalContext::new(
            vec!["o1".into(), "o2".into(), "o3".into()],
            vec!["a".into(), "b".into(), "c".into()],
            vec![
                (0, 0),
                (0, 1), // o1: a,b
                (1, 0),
                (1, 2), // o2: a,c
                (2, 0),
                (2, 1),
                (2, 2), // o3: a,b,c
            ],
        )
    }

    fn intent_labels(ctx: &FormalContext, c: &Concept) -> Vec<String> {
        ctx.label_attrs(&c.intent)
    }
    fn extent_set(ctx: &FormalContext, c: &Concept) -> Vec<String> {
        c.extent.iter().map(|g| ctx.objects[g].clone()).collect()
    }

    #[test]
    fn derivation_operators_are_a_galois_connection() {
        let ctx = ctx_3x3();
        // {a,b}' = {o1,o3}; {o1,o3}' = {a,b} (closed).
        let mut ab = BitSet::empty(3);
        ab.insert(0);
        ab.insert(1);
        let ext = ctx.extent_of(&ab);
        assert_eq!(
            ext.iter().collect::<Vec<_>>(),
            vec![0, 2],
            "{{a,b}}' = {{o1,o3}}"
        );
        let closed = ctx.intent_of(&ext);
        assert_eq!(closed, ab, "{{o1,o3}}' = {{a,b}} → closed");
        // Empty intent derives all objects (universal extent).
        assert_eq!(ctx.extent_of(&BitSet::empty(3)).count(), 3);
    }

    #[test]
    fn nextclosure_enumerates_the_exact_concept_set() {
        let ctx = ctx_3x3();
        let (concepts, truncated) = ctx.concepts(100);
        assert!(!truncated);
        // Exactly four concepts, as hand-derived.
        let intents: Vec<Vec<String>> = concepts.iter().map(|c| intent_labels(&ctx, c)).collect();
        assert_eq!(concepts.len(), 4, "intents found: {intents:?}");

        // Top concept: all objects, intent {a}.
        let top = &concepts[0];
        assert_eq!(extent_set(&ctx, top), vec!["o1", "o2", "o3"]);
        assert_eq!(intent_labels(&ctx, top), vec!["a"]);

        // The full set of intents must match exactly.
        let mut got = intents.clone();
        got.sort();
        let mut want = vec![
            vec!["a".to_string()],
            vec!["a".to_string(), "b".to_string()],
            vec!["a".to_string(), "c".to_string()],
            vec!["a".to_string(), "b".to_string(), "c".to_string()],
        ];
        want.sort();
        assert_eq!(got, want, "concept intents must match the hand derivation");

        // Bottom concept (full intent {a,b,c}) has extent {o3}.
        let bottom = concepts
            .iter()
            .find(|c| intent_labels(&ctx, c) == vec!["a", "b", "c"])
            .expect("bottom concept present");
        assert_eq!(extent_set(&ctx, bottom), vec!["o3"]);
    }

    #[test]
    fn hasse_covers_match_the_diamond() {
        let ctx = ctx_3x3();
        let (concepts, _) = ctx.concepts(100);
        let covers = FormalContext::covers(&concepts);
        // Index lookup by intent for a label-stable assertion.
        let idx = |labels: &[&str]| {
            concepts
                .iter()
                .position(|c| {
                    intent_labels(&ctx, c)
                        == labels.iter().map(|s| s.to_string()).collect::<Vec<_>>()
                })
                .unwrap_or_else(|| panic!("no concept with intent {labels:?}"))
        };
        let top = idx(&["a"]);
        let ab = idx(&["a", "b"]);
        let ac = idx(&["a", "c"]);
        let bot = idx(&["a", "b", "c"]);

        // Diamond: top ⋗ {ab, ac} ⋗ bottom. Edges are (child, parent).
        let want: Vec<(usize, usize)> = {
            let mut v = vec![(ab, top), (ac, top), (bot, ab), (bot, ac)];
            v.sort_unstable();
            v
        };
        assert_eq!(covers, want, "Hasse cover must be the concept diamond");
    }

    #[test]
    fn implications_capture_a_known_attribute_dependency() {
        // In this context, b ⟹ a and c ⟹ a (a is shared by everyone), and the
        // bottom adds the complementary attribute. The cover-derived implications
        // include {a,b} ⟹ {c}? No — {a,b}'={o1,o3}, adding c drops to {o3}, so the
        // edge bottom⋖{a,b} yields premise {a,b} ⟹ {c}. Symmetrically {a,c} ⟹ {b}.
        let ctx = ctx_3x3();
        let (concepts, _) = ctx.concepts(100);
        let covers = FormalContext::covers(&concepts);
        let imps = ctx.implications(&concepts, &covers);

        let has = |prem: &[&str], concl: &[&str]| {
            imps.iter().any(|im| {
                im.premise == prem.iter().map(|s| s.to_string()).collect::<Vec<_>>()
                    && im.conclusion == concl.iter().map(|s| s.to_string()).collect::<Vec<_>>()
            })
        };
        assert!(
            has(&["a", "b"], &["c"]),
            "expected {{a,b}} ⟹ {{c}}; got {imps:?}"
        );
        assert!(
            has(&["a", "c"], &["b"]),
            "expected {{a,c}} ⟹ {{b}}; got {imps:?}"
        );
        // Support of {a,b} ⟹ {c} is |{a,b}'| = |{o1,o3}| = 2.
        let ab_c = imps
            .iter()
            .find(|im| im.premise == vec!["a", "b"] && im.conclusion == vec!["c"])
            .expect("implication present");
        assert_eq!(ab_c.support, 2);
    }

    #[test]
    fn truncation_flag_is_set_when_capped() {
        let ctx = ctx_3x3();
        let (concepts, truncated) = ctx.concepts(2);
        assert!(truncated, "cap of 2 must truncate the 4-concept lattice");
        assert_eq!(concepts.len(), 2);
    }

    #[test]
    fn empty_incidence_yields_top_and_bottom() {
        // No incidence over a non-empty attribute set: two concepts.
        //   ⊤ = (all objects, ∅)              — ∅' = all objects, (all)' = ∅
        //   ⊥ = (∅,           {p,q})          — {p,q}' = ∅,        ∅' = {p,q}
        // The bottom exists because the full attribute set is closed (its extent
        // is empty, and the empty extent's intent is the full attribute set).
        let ctx = FormalContext::new(
            vec!["x".into(), "y".into()],
            vec!["p".into(), "q".into()],
            Vec::<(usize, usize)>::new(),
        );
        let (concepts, truncated) = ctx.concepts(100);
        assert!(!truncated);
        assert_eq!(concepts.len(), 2, "top (all, ∅) and bottom (∅, {{p,q}})");
        // Top: lectically first (smallest intent ∅).
        assert_eq!(concepts[0].extent.count(), 2);
        assert_eq!(concepts[0].intent.count(), 0);
        // Bottom: full intent, empty extent.
        let bottom = concepts
            .iter()
            .find(|c| c.intent.count() == 2)
            .expect("bottom concept present");
        assert_eq!(bottom.extent.count(), 0);
    }

    #[test]
    fn no_attributes_yields_single_concept() {
        // Degenerate: zero attributes → the only closed set is ∅, one concept
        // (all objects, ∅). Guards the `full == empty` start condition.
        let ctx = FormalContext::new(
            vec!["x".into(), "y".into()],
            Vec::<String>::new(),
            Vec::<(usize, usize)>::new(),
        );
        let (concepts, truncated) = ctx.concepts(100);
        assert!(!truncated);
        assert_eq!(concepts.len(), 1);
        assert_eq!(concepts[0].extent.count(), 2);
        assert_eq!(concepts[0].intent.count(), 0);
    }
}
