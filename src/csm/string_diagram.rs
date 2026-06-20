//! Monoidal **string-diagram decomposition** of a Multiparty-Session-Type
//! [`GlobalType`] (ADR-028, CT-3).
//!
//! A protocol is read as a morphism in a (strict) monoidal category whose
//! objects are *role wires* and whose generating morphisms are the
//! interactions. The decomposition this module COMPUTES is the actionable,
//! falsifiable payoff — the rendered diagram is secondary:
//!
//! - **Sequential composition** (`;`) is the consecutive-interaction *spine*:
//!   `sequential_depth` counts the interaction steps a single trace performs
//!   (a `Choice` contributes one step — the selection message — plus its
//!   longest branch, since exactly one branch runs per execution).
//! - **Tensor** (`⊗`) is the partition of roles into *independent parallel
//!   sub-protocols*. Two roles are unioned (union-find) whenever they appear
//!   together in **any** `Interaction` (the `from`/`to` pair) or **any**
//!   `Choice` (the selector `from`/`to` pair). Each connected component is a
//!   tensor factor. Two roles in DIFFERENT factors provably never communicate
//!   in this protocol — a schedule-relevant, checkable claim (they can be run
//!   on independent executors with no message between them).
//!
//! Everything here is pure data → data; it depends only on the AST so it
//! unit-tests without a database. The `csm_protocol_string_diagram` MCP tool
//! (`crate::mcp::tools::tool_csm_protocol_string_diagram`) loads a real
//! `csm_protocols` row, calls [`decompose`], and serializes the result.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::csm::mpst::global::GlobalType;
use crate::csm::role::Role;

/// One generating morphism of the diagram: a single message box rendered between
/// the `from` and `to` wires at sequence position `seq_index`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiagramBox {
    /// Position along the sequential spine (0-based, pre-order over the AST).
    pub seq_index: usize,
    /// `"interaction"` for a plain message, `"choice"` for a branch-selector
    /// message (one box per branch label of a `Choice`).
    pub kind: String,
    /// Sender role (the upper/left wire of the box).
    pub from: String,
    /// Receiver role (the lower/right wire of the box).
    pub to: String,
    /// The message label carried by this box.
    pub label: String,
}

/// Recursion summary: whether the protocol has any `Rec` binder and the names of
/// every bound recursion variable (the back-edges in the diagram).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecursionInfo {
    pub has_rec: bool,
    pub vars: Vec<String>,
}

/// The full computed decomposition returned to the tool body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Decomposition {
    /// All participating roles, sorted (the wires).
    pub roles: Vec<String>,
    /// Every message box in pre-order.
    pub boxes: Vec<DiagramBox>,
    /// The tensor partition: each inner vector is one independent factor's role
    /// set (sorted); factors are ordered by their lexicographically-smallest
    /// role so the output is deterministic.
    pub tensor_factors: Vec<Vec<String>>,
    /// `tensor_factors.len()` — the number of independent parallel sub-protocols.
    pub n_tensor_factors: usize,
    /// The sequential depth (interaction steps on the longest single trace).
    pub sequential_depth: usize,
    /// Recursion summary.
    pub recursion: RecursionInfo,
}

/// A minimal disjoint-set (union-find) over role indices with path compression
/// and union-by-size. Used to compute the tensor partition in near-linear time.
struct UnionFind {
    parent: Vec<usize>,
    size: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        UnionFind {
            parent: (0..n).collect(),
            size: vec![1; n],
        }
    }

    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]]; // halving
            x = self.parent[x];
        }
        x
    }

    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra == rb {
            return;
        }
        // Attach the smaller tree under the larger.
        let (small, large) = if self.size[ra] < self.size[rb] {
            (ra, rb)
        } else {
            (rb, ra)
        };
        self.parent[small] = large;
        self.size[large] += self.size[small];
    }
}

/// Compute the monoidal decomposition of `g`.
///
/// This is the entry point used by the MCP tool and the tests. It performs three
/// passes over the AST (roles, boxes + co-occurrence unions, sequential depth)
/// and is `O(n)` in the size of the protocol plus the near-linear union-find.
pub fn decompose(g: &GlobalType) -> Decomposition {
    // 1. Roles → stable indices for the union-find.
    let role_set: BTreeSet<Role> = g.participants();
    let roles: Vec<String> = role_set.iter().map(|r| r.to_string()).collect();
    let index_of: BTreeMap<String, usize> = roles
        .iter()
        .enumerate()
        .map(|(i, r)| (r.clone(), i))
        .collect();

    let mut uf = UnionFind::new(roles.len());
    let mut boxes: Vec<DiagramBox> = Vec::new();
    let mut rec_vars: BTreeSet<String> = BTreeSet::new();
    let mut seq = 0usize;

    collect(g, &index_of, &mut uf, &mut boxes, &mut rec_vars, &mut seq);

    // 2. Materialize the tensor partition from the union-find components.
    let tensor_factors = components(&roles, &index_of, &mut uf);

    // 3. Sequential depth = interaction steps on the longest single trace.
    let sequential_depth = depth(g);

    let vars: Vec<String> = rec_vars.iter().cloned().collect();
    Decomposition {
        n_tensor_factors: tensor_factors.len(),
        roles,
        boxes,
        tensor_factors,
        sequential_depth,
        recursion: RecursionInfo {
            has_rec: !vars.is_empty(),
            vars,
        },
    }
}

/// Pre-order walk: emit a box per message, union the role pair of every
/// `Interaction`/`Choice`, and record recursion-variable names.
fn collect(
    g: &GlobalType,
    index_of: &BTreeMap<String, usize>,
    uf: &mut UnionFind,
    boxes: &mut Vec<DiagramBox>,
    rec_vars: &mut BTreeSet<String>,
    seq: &mut usize,
) {
    match g {
        GlobalType::Interaction {
            from,
            to,
            label,
            cont,
        } => {
            union_pair(from, to, index_of, uf);
            boxes.push(DiagramBox {
                seq_index: *seq,
                kind: "interaction".to_string(),
                from: from.to_string(),
                to: to.to_string(),
                label: label.name.clone(),
            });
            *seq += 1;
            collect(cont, index_of, uf, boxes, rec_vars, seq);
        }
        GlobalType::Choice { from, to, branches } => {
            union_pair(from, to, index_of, uf);
            for b in branches {
                boxes.push(DiagramBox {
                    seq_index: *seq,
                    kind: "choice".to_string(),
                    from: from.to_string(),
                    to: to.to_string(),
                    label: b.label.name.clone(),
                });
                *seq += 1;
                collect(&b.cont, index_of, uf, boxes, rec_vars, seq);
            }
        }
        GlobalType::Rec { var, body } => {
            rec_vars.insert(var.clone());
            collect(body, index_of, uf, boxes, rec_vars, seq);
        }
        GlobalType::Var { .. } | GlobalType::End => {}
    }
}

/// Union the indices of two roles (both are always in `index_of` because
/// `participants()` collected them).
fn union_pair(a: &Role, b: &Role, index_of: &BTreeMap<String, usize>, uf: &mut UnionFind) {
    if let (Some(&ia), Some(&ib)) = (index_of.get(a.as_str()), index_of.get(b.as_str())) {
        uf.union(ia, ib);
    }
}

/// Materialize the union-find components into sorted role-sets, ordered by their
/// smallest member for deterministic output.
fn components(
    roles: &[String],
    index_of: &BTreeMap<String, usize>,
    uf: &mut UnionFind,
) -> Vec<Vec<String>> {
    let mut groups: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for role in roles {
        let idx = index_of[role];
        let root = uf.find(idx);
        groups.entry(root).or_default().push(role.clone());
    }
    // BTreeMap over the (sorted) `roles` keeps each factor's members sorted and
    // the factors ordered by first-seen root; re-sort factors by their min role
    // so the ordering is intrinsic (not dependent on union-find internal roots).
    let mut factors: Vec<Vec<String>> = groups.into_values().collect();
    factors.sort_by(|a, b| a.first().cmp(&b.first()));
    factors
}

/// Sequential depth: the number of interaction steps along the **longest single
/// execution trace**. A `Choice` runs exactly one branch, so it contributes one
/// step (the selector message) plus the max over its branches. `Rec` is
/// transparent (the back-edge is a loop, not extra linear depth); `Var`/`End`
/// are 0. For a purely sequential protocol this equals the interaction count.
fn depth(g: &GlobalType) -> usize {
    match g {
        GlobalType::Interaction { cont, .. } => 1 + depth(cont),
        GlobalType::Choice { branches, .. } => {
            1 + branches.iter().map(|b| depth(&b.cont)).max().unwrap_or(0)
        }
        GlobalType::Rec { body, .. } => depth(body),
        GlobalType::Var { .. } | GlobalType::End => 0,
    }
}

/// Render a unicode monoidal string diagram of the decomposition.
///
/// Wires are vertical role lines (one column per role, grouped by tensor
/// factor with a gap between factors). Each interaction/choice box is a
/// horizontal connector `●──▶○` drawn on its own row between the sender and
/// receiver columns, annotated with the label. A `Rec` back-edge is annotated
/// as a trailing `↺ μvar` note. This is a faithful *secondary* visualization of
/// the structure already computed in [`Decomposition`].
pub fn render(name: &str, d: &Decomposition) -> String {
    let mut out = String::new();
    out.push_str(&format!("string diagram :: {name}\n"));

    // Header: the tensor factorization as `[A B] ⊗ [C D]`.
    if d.tensor_factors.is_empty() {
        out.push_str("(no roles)\n");
        return out;
    }
    let factor_strs: Vec<String> = d
        .tensor_factors
        .iter()
        .map(|f| format!("[{}]", f.join(" ")))
        .collect();
    out.push_str(&format!(
        "tensor ({} factor{}): {}\n",
        d.n_tensor_factors,
        if d.n_tensor_factors == 1 { "" } else { "s" },
        factor_strs.join(" ⊗ ")
    ));
    out.push_str(&format!("sequential depth: {}\n", d.sequential_depth));

    // Column layout: roles in factor order, a blank spacer column between
    // factors. `col_of[role] = x position (in characters)`.
    let mut col_of: BTreeMap<&str, usize> = BTreeMap::new();
    let mut header = String::new();
    let mut x = 0usize;
    for (fi, factor) in d.tensor_factors.iter().enumerate() {
        if fi > 0 {
            header.push_str("    "); // 4-char gap marks the ⊗ boundary
            x += 4;
        }
        for role in factor {
            // Each wire occupies the role name's width + 2 padding.
            let label = role.as_str();
            col_of.insert(label, x + label.len() / 2);
            header.push_str(label);
            header.push_str("  ");
            x += label.len() + 2;
        }
    }
    let width = x.max(1);
    out.push_str(&header);
    out.push('\n');

    // A wire row: a `│` under every role column.
    let wire_row = |cols: &BTreeMap<&str, usize>| -> String {
        let mut row = vec![' '; width];
        for &c in cols.values() {
            if c < width {
                row[c] = '│';
            }
        }
        row.into_iter().collect::<String>()
    };

    // Draw each box on its own row, with a leading wire row for breathing space.
    for b in &d.boxes {
        out.push_str(&wire_row(&col_of));
        out.push('\n');

        let (Some(&cf), Some(&ct)) = (col_of.get(b.from.as_str()), col_of.get(b.to.as_str()))
        else {
            continue;
        };
        let (lo, hi) = (cf.min(ct), cf.max(ct));
        let mut row: Vec<char> = vec![' '; width];
        // Wires that are not endpoints still pass through this row.
        for (role, &c) in &col_of {
            if *role != b.from && *role != b.to && c < width {
                row[c] = '│';
            }
        }
        // The connector between sender and receiver.
        for cell in row[lo..=hi].iter_mut() {
            *cell = '─';
        }
        let sender_glyph = if b.kind == "choice" { '◆' } else { '●' };
        row[cf] = sender_glyph; // sender endpoint (filled / diamond for choice)
        if ct < width {
            // direction arrowhead at the receiver
            row[ct] = if ct >= cf { '▶' } else { '◀' };
        }
        let mut line: String = row.into_iter().collect();
        line.push_str(&format!("  {} : {} → {}", b.label, b.from, b.to));
        out.push_str(&line);
        out.push('\n');
    }

    // Closing wire row.
    out.push_str(&wire_row(&col_of));
    out.push('\n');

    if d.recursion.has_rec {
        out.push_str(&format!(
            "↺ back-edge(s): {} (recursion μ-binder(s) loop the spine)\n",
            d.recursion.vars.join(", ")
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csm::mpst::global::{choice, end, gbranch, interaction, rec, var};
    use crate::csm::role::Label;

    /// O → R : q . R → O : a . end — purely sequential, one tensor factor,
    /// depth == #interactions (2).
    #[test]
    fn sequential_protocol_single_factor() {
        let g = interaction(
            "O",
            "R",
            Label::text("q"),
            interaction("R", "O", Label::text("a"), end()),
        );
        let d = decompose(&g);
        assert_eq!(d.n_tensor_factors, 1, "all roles communicate → one factor");
        assert_eq!(
            d.tensor_factors,
            vec![vec!["O".to_string(), "R".to_string()]]
        );
        assert_eq!(
            d.sequential_depth, 2,
            "two interactions on the single trace"
        );
        assert_eq!(d.boxes.len(), 2);
        assert!(!d.recursion.has_rec);
        // Render must not panic and must mention the single factor.
        let s = render("seq", &d);
        assert!(s.contains("1 factor"), "diagram: {s}");
    }

    /// (A → B) and (C → D) with NO cross edge: roles split into two
    /// non-interacting pairs → exactly two tensor factors. A and C provably
    /// never communicate.
    #[test]
    fn two_independent_pairs_two_factors() {
        // A→B:m1 . C→D:m2 . end. Although written sequentially, {A,B} and {C,D}
        // share no interaction, so they are independent tensor factors.
        let g = interaction(
            "A",
            "B",
            Label::text("m1"),
            interaction("C", "D", Label::text("m2"), end()),
        );
        let d = decompose(&g);
        assert_eq!(d.n_tensor_factors, 2, "two non-interacting pairs");
        assert_eq!(
            d.tensor_factors,
            vec![
                vec!["A".to_string(), "B".to_string()],
                vec!["C".to_string(), "D".to_string()],
            ]
        );
        // Falsifiable claim: A and C are in different factors.
        let factor_of = |role: &str| {
            d.tensor_factors
                .iter()
                .position(|f| f.iter().any(|r| r == role))
                .expect("role present")
        };
        assert_ne!(
            factor_of("A"),
            factor_of("C"),
            "A and C must be in different factors (never communicate)"
        );
        let s = render("par", &d);
        assert!(s.contains("⊗"), "two factors should render a ⊗: {s}");
    }

    /// A Rec protocol → recursion.has_rec == true and the var is recorded.
    #[test]
    fn recursive_protocol_flags_rec() {
        // μloop. O → R : ping . loop
        let g = rec(
            "loop",
            interaction("O", "R", Label::text("ping"), var("loop")),
        );
        let d = decompose(&g);
        assert!(d.recursion.has_rec, "Rec must be detected");
        assert_eq!(d.recursion.vars, vec!["loop".to_string()]);
        assert_eq!(d.n_tensor_factors, 1);
        // Depth counts the single interaction in the loop body (back-edge adds 0).
        assert_eq!(d.sequential_depth, 1);
        let s = render("rec", &d);
        assert!(
            s.contains("back-edge"),
            "rec should annotate a back-edge: {s}"
        );
    }

    /// A Choice contributes one selector step plus its longest branch; a bystander
    /// role that shares no edge stays in its own factor.
    #[test]
    fn choice_depth_and_factors() {
        // O → C { pass: end, revise: O → W : redo . end } , and an isolated pair X→Y.
        let g = interaction(
            "X",
            "Y",
            Label::text("aside"),
            choice(
                "O",
                "C",
                vec![
                    gbranch(Label::text("pass"), end()),
                    gbranch(
                        Label::text("revise"),
                        interaction("O", "W", Label::text("redo"), end()),
                    ),
                ],
            ),
        );
        let d = decompose(&g);
        // {X,Y} independent of {O,C,W}.
        assert_eq!(d.n_tensor_factors, 2);
        // Longest trace through the choice: X→Y (1) + selector (1) + revise branch (1) = 3.
        assert_eq!(d.sequential_depth, 3);
        // Two choice boxes (one per branch label) + 2 interactions.
        assert_eq!(
            d.boxes.iter().filter(|b| b.kind == "choice").count(),
            2,
            "one box per branch label"
        );
    }
}
