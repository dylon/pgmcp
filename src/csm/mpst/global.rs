//! The Multiparty Session Types **global type**: a bird's-eye description of a
//! whole protocol (who sends what to whom, in what order, with what choices and
//! recursion). Projection (`super::project`) derives one [`LocalType`] per role.
//!
//! [`LocalType`]: super::local::LocalType
//!
//! # ADR-006 — adjacent tagging is mandatory
//!
//! [`GlobalType`] is **recursive** (`Rec`/`Var` + boxed `cont`). It therefore
//! uses `#[serde(tag = "type", content = "data")]` (adjacent), never internal
//! `#[serde(tag = "type")]`, which stalls rustc's monomorphization collector for
//! ~2h on recursive enums. Do not change this without re-reading ADR-006.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::csm::role::{Label, Role};

/// A recursion variable name (`μ var. …` binds it; `Var { var }` references it).
pub type TypeVar = String;

/// A global protocol type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")] // ADR-006: ADJACENT
pub enum GlobalType {
    /// `from → to : label . cont` — a single message then the continuation.
    Interaction {
        from: Role,
        to: Role,
        label: Label,
        cont: Box<GlobalType>,
    },
    /// `from → to { labelᵢ : Gᵢ }` — sender-driven choice: `from` selects one
    /// label and sends it to `to`, then both continue as the chosen branch.
    Choice {
        from: Role,
        to: Role,
        branches: Vec<GlobalBranch>,
    },
    /// `μ var. body` — recursion binder.
    Rec { var: TypeVar, body: Box<GlobalType> },
    /// `var` — a reference to an enclosing `Rec`'s variable (a back-edge).
    Var { var: TypeVar },
    /// `end` — protocol completion.
    End,
}

/// One arm of a [`GlobalType::Choice`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalBranch {
    pub label: Label,
    pub cont: GlobalType,
}

impl GlobalType {
    /// The set of roles that appear anywhere in this protocol.
    pub fn participants(&self) -> BTreeSet<Role> {
        let mut acc = BTreeSet::new();
        self.collect_roles(&mut acc);
        acc
    }

    fn collect_roles(&self, acc: &mut BTreeSet<Role>) {
        match self {
            GlobalType::Interaction { from, to, cont, .. } => {
                acc.insert(from.clone());
                acc.insert(to.clone());
                cont.collect_roles(acc);
            }
            GlobalType::Choice {
                from, to, branches, ..
            } => {
                acc.insert(from.clone());
                acc.insert(to.clone());
                for b in branches {
                    b.cont.collect_roles(acc);
                }
            }
            GlobalType::Rec { body, .. } => body.collect_roles(acc),
            GlobalType::Var { .. } | GlobalType::End => {}
        }
    }

    /// Every communication `(from, to, label-name)` the protocol can emit — its
    /// declared alphabet (used by Phase-8 inference to flag novel/off-protocol
    /// peer behaviour).
    pub fn communications(&self) -> Vec<(String, String, String)> {
        let mut acc = Vec::new();
        self.collect_comms(&mut acc);
        acc
    }

    fn collect_comms(&self, acc: &mut Vec<(String, String, String)>) {
        match self {
            GlobalType::Interaction {
                from,
                to,
                label,
                cont,
            } => {
                acc.push((from.to_string(), to.to_string(), label.name.clone()));
                cont.collect_comms(acc);
            }
            GlobalType::Choice { from, to, branches } => {
                for b in branches {
                    acc.push((from.to_string(), to.to_string(), b.label.name.clone()));
                    b.cont.collect_comms(acc);
                }
            }
            GlobalType::Rec { body, .. } => body.collect_comms(acc),
            GlobalType::Var { .. } | GlobalType::End => {}
        }
    }

    /// Sequential composition `self ; cont` — the protocol-composition algebra's
    /// product (Crucible CT-2). Grafts `cont` onto every `End` leaf of `self`, so
    /// the composite runs `self` to completion and then continues as `cont`.
    ///
    /// `End` is the unit (`g.then(End) == g`, `End.then(g) == g`), the operation
    /// is associative, and it is CLOSED — composing two well-formed protocols
    /// yields a well-formed protocol. These laws are mechanically proved in
    /// `docs/formal/rocq/CsmMpst.v` (`gseq`, `gseq_unit_l`/`gseq_unit_r`,
    /// `gseq_assoc`, `wf_gseq`), and projection is a monoid homomorphism over it
    /// (`project_gseq_hom`: `project(p;q) = project(p) ; project(q)`) — which is
    /// exactly what lets `csm_synthesize_protocol` fold a plan subtree into one
    /// drivable GlobalType and still project sound per-role machines. A loop's
    /// `End` exit arms continue as `cont`; `Var` back-edges keep looping, so
    /// `cont` must be closed (no free `Var`) — the Orchestrator only composes
    /// closed sub-protocols.
    pub fn then(self, cont: GlobalType) -> GlobalType {
        match self {
            GlobalType::End => cont,
            GlobalType::Interaction {
                from,
                to,
                label,
                cont: k,
            } => GlobalType::Interaction {
                from,
                to,
                label,
                cont: Box::new(k.then(cont)),
            },
            GlobalType::Choice { from, to, branches } => GlobalType::Choice {
                from,
                to,
                branches: branches
                    .into_iter()
                    .map(|b| GlobalBranch {
                        label: b.label,
                        cont: b.cont.then(cont.clone()),
                    })
                    .collect(),
            },
            GlobalType::Rec { var, body } => GlobalType::Rec {
                var,
                body: Box::new(body.then(cont)),
            },
            GlobalType::Var { var } => GlobalType::Var { var },
        }
    }
}

// ── Ergonomic constructors (keep protocol literals readable) ──────────────────

/// `from → to : label . cont`
pub fn interaction(
    from: impl Into<Role>,
    to: impl Into<Role>,
    label: Label,
    cont: GlobalType,
) -> GlobalType {
    GlobalType::Interaction {
        from: from.into(),
        to: to.into(),
        label,
        cont: Box::new(cont),
    }
}

/// `from → to { branches }`
pub fn choice(
    from: impl Into<Role>,
    to: impl Into<Role>,
    branches: Vec<GlobalBranch>,
) -> GlobalType {
    GlobalType::Choice {
        from: from.into(),
        to: to.into(),
        branches,
    }
}

/// One `label : cont` arm of a choice.
pub fn gbranch(label: Label, cont: GlobalType) -> GlobalBranch {
    GlobalBranch { label, cont }
}

/// `μ var. body`
pub fn rec(var: impl Into<TypeVar>, body: GlobalType) -> GlobalType {
    GlobalType::Rec {
        var: var.into(),
        body: Box::new(body),
    }
}

/// `var`
pub fn var(var: impl Into<TypeVar>) -> GlobalType {
    GlobalType::Var { var: var.into() }
}

/// `end`
pub fn end() -> GlobalType {
    GlobalType::End
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csm::role::Label;

    #[test]
    fn then_is_sequential_composition_and_preserves_well_formedness() {
        use crate::csm::mpst::wellformed::well_formed;
        // p = O → A : x . end     q = A → O : y . end   (both well-formed)
        let p = interaction("O", "A", Label::text("x"), end());
        let q = interaction("A", "O", Label::text("y"), end());
        assert!(well_formed(&p).is_ok());
        assert!(well_formed(&q).is_ok());

        // End is the two-sided unit (gseq_unit_l / gseq_unit_r).
        assert_eq!(p.clone().then(end()), p);
        assert_eq!(end().then(q.clone()), q);

        // p ; q grafts q onto p's End leaf: O→A:x . A→O:y . end.
        let pq = p.clone().then(q.clone());
        assert_eq!(
            pq,
            interaction(
                "O",
                "A",
                Label::text("x"),
                interaction("A", "O", Label::text("y"), end()),
            )
        );

        // CLOSURE (wf_gseq): the composite of two well-formed protocols is
        // well-formed.
        assert!(well_formed(&pq).is_ok());

        // Associativity (gseq_assoc): (p;q);r == p;(q;r).
        let r = interaction("O", "A", Label::text("z"), end());
        assert_eq!(
            p.clone().then(q.clone()).then(r.clone()),
            p.clone().then(q.clone().then(r.clone())),
        );
    }

    #[test]
    fn participants_are_collected() {
        // O → R : q . R → O : a . end
        let g = interaction(
            "O",
            "R",
            Label::text("q"),
            interaction("R", "O", Label::text("a"), end()),
        );
        let ps = g.participants();
        assert_eq!(ps.len(), 2);
        assert!(ps.contains(&Role::new("O")));
        assert!(ps.contains(&Role::new("R")));
    }

    #[test]
    fn recursive_global_type_round_trips_via_adjacent_tagging() {
        // The ADR-006 canary: a *recursive* enum that compiles fast and
        // serialises with the adjacent `{"type":..,"data":..}` shape.
        let g = rec("t", interaction("O", "R", Label::text("ping"), var("t")));
        let json = serde_json::to_string(&g).expect("serialize");
        assert!(
            json.contains(r#""type":"rec""#),
            "expected adjacent tag: {json}"
        );
        assert!(json.contains(r#""type":"interaction""#));
        assert!(json.contains(r#""type":"var""#));
        let back: GlobalType = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, g);
    }
}
