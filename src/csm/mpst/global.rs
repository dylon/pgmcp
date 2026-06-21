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

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::csm::role::{Label, Role, StackAction};

/// A recursion variable name (`μ var. …` binds it; `Var { var }` references it).
pub type TypeVar = String;

/// A reference to a *named* sub-protocol in the registry — the RSM "callee" of a
/// [`GlobalType::GlobalCall`]. Holds the name only (not a DB foreign key); the
/// callee is resolved through [`crate::csm::registry`] at well-formedness and
/// compile time. A name (rather than an inlined body) is what lets a protocol
/// reference *itself* — finite syntax for unbounded recursion (the RLM
/// `RecursiveCf`), which an inline [`GlobalType::GlobalBox`] cannot express.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProtocolRef {
    pub name: String,
}

impl ProtocolRef {
    pub fn new(name: impl Into<String>) -> Self {
        ProtocolRef { name: name.into() }
    }
}

/// An environment resolving named sub-protocols ([`ProtocolRef`]) to their global
/// types — the registry a [`GlobalType::GlobalCall`] is checked, projected, and
/// compiled against. Lives in the `mpst` layer (below
/// [`crate::csm::registry`]) so well-formedness/projection/compilation can depend
/// on it without a module cycle. A self-recursive protocol is represented by a
/// single entry whose body calls its own name.
#[derive(Debug, Clone, Default)]
pub struct ProtocolEnv {
    protocols: BTreeMap<String, GlobalType>,
}

impl ProtocolEnv {
    pub fn new() -> Self {
        ProtocolEnv::default()
    }

    /// Register (or replace) a named sub-protocol.
    pub fn insert(&mut self, name: impl Into<String>, g: GlobalType) {
        self.protocols.insert(name.into(), g);
    }

    /// Resolve a reference to its global type, if registered.
    pub fn resolve(&self, r: &ProtocolRef) -> Option<&GlobalType> {
        self.protocols.get(&r.name)
    }

    pub fn contains(&self, r: &ProtocolRef) -> bool {
        self.protocols.contains_key(&r.name)
    }

    pub fn len(&self) -> usize {
        self.protocols.len()
    }

    pub fn is_empty(&self) -> bool {
        self.protocols.is_empty()
    }
}

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
    /// `call C[σ] . cont` — a Recursive-State-Machine **call** to the named
    /// sub-protocol `callee`, with its roles renamed by `subst` (callee-role →
    /// caller-role), then continue as `cont` *after the callee returns* (Alur et
    /// al., *Analysis of Recursive State Machines*, TOPLAS 2005). Entering pushes
    /// a return frame; the callee's `End` pops it and resumes `cont`. The explicit
    /// `cont` return-continuation is precisely what `Rec`/`Var` (a tail back-edge)
    /// cannot express — this is the context-free construct. Because `callee` is a
    /// *name*, a protocol may call itself (unbounded nesting from finite syntax).
    GlobalCall {
        callee: ProtocolRef,
        subst: BTreeMap<Role, Role>,
        cont: Box<GlobalType>,
    },
    /// `box⟨enter⟩{ body }⟨exit⟩ . cont` — an *inline* hierarchical sub-region (a
    /// Harel composite state, realized as an RSM box): on `enter` push a frame,
    /// run the inline `body`, on its `End` emit `exit` and pop, then continue as
    /// `cont`. Unlike [`GlobalType::GlobalCall`] the body is inlined (so it cannot
    /// recurse), which is the right shape for one-shot nesting / bounded sub-plans;
    /// `enter`/`exit` are the visibly-pushdown push/pop boundary symbols.
    GlobalBox {
        enter: Label,
        body: Box<GlobalType>,
        exit: Label,
        cont: Box<GlobalType>,
    },
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
            GlobalType::GlobalCall { subst, cont, .. } => {
                // The caller-side roles playing in this frame are the substitution
                // image; the callee's own role names are renamed away and never
                // appear in the caller's role space. (WF-THREAD makes `subst` total
                // over the callee's participants, so this captures every frame role.)
                for r in subst.values() {
                    acc.insert(r.clone());
                }
                cont.collect_roles(acc);
            }
            GlobalType::GlobalBox { body, cont, .. } => {
                // The body is inline (same role space as the caller), so its roles
                // are part of this protocol; then the post-return continuation.
                body.collect_roles(acc);
                cont.collect_roles(acc);
            }
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
            // A call's internal communications live in the callee's own definition
            // (resolved via the registry at compile time); the `call:`/`ret:`
            // boundary symbols are stack-control, surfaced by `alphabet()` rather
            // than as wire communications. So only the return continuation is local.
            GlobalType::GlobalCall { cont, .. } => cont.collect_comms(acc),
            // A box's body is inline, so its communications ARE part of this
            // protocol; the `enter`/`exit` boundary symbols are likewise stack-
            // control (see `alphabet()`), not (from,to,label) wire communications.
            GlobalType::GlobalBox { body, cont, .. } => {
                body.collect_comms(acc);
                cont.collect_comms(acc);
            }
            GlobalType::Var { .. } | GlobalType::End => {}
        }
    }

    /// The protocol's **visibly-pushdown alphabet**: each symbol paired with the
    /// [`StackAction`] it triggers (Σ_int = `Neutral`, Σ_call = `Push`, Σ_ret =
    /// `Pop`). This is what the conformance engine ([`crate::csm::conformance`])
    /// consumes to build the per-role pushdown automaton, and what the v54
    /// `csm_protocol_alphabet` table persists. Ordinary `Interaction`/`Choice`
    /// labels are `Neutral`; a `GlobalCall` contributes the reserved `call:<name>`
    /// (`Push`) and `ret:<name>` (`Pop`) boundary symbols; a `GlobalBox`
    /// contributes its explicit `enter` (`Push`) and `exit` (`Pop`) labels. A
    /// callee's *internal* alphabet is the callee's own (composed via the
    /// registry), so it is not repeated here.
    pub fn alphabet(&self) -> Vec<(String, StackAction)> {
        let mut acc = Vec::new();
        self.collect_alphabet(&mut acc);
        acc
    }

    fn collect_alphabet(&self, acc: &mut Vec<(String, StackAction)>) {
        match self {
            GlobalType::Interaction { label, cont, .. } => {
                acc.push((label.name.clone(), StackAction::Neutral));
                cont.collect_alphabet(acc);
            }
            GlobalType::Choice { branches, .. } => {
                for b in branches {
                    acc.push((b.label.name.clone(), StackAction::Neutral));
                    b.cont.collect_alphabet(acc);
                }
            }
            GlobalType::Rec { body, .. } => body.collect_alphabet(acc),
            GlobalType::GlobalCall { callee, cont, .. } => {
                acc.push((format!("call:{}", callee.name), StackAction::Push));
                acc.push((format!("ret:{}", callee.name), StackAction::Pop));
                cont.collect_alphabet(acc);
            }
            GlobalType::GlobalBox {
                enter,
                body,
                exit,
                cont,
            } => {
                acc.push((enter.name.clone(), StackAction::Push));
                body.collect_alphabet(acc);
                acc.push((exit.name.clone(), StackAction::Pop));
                cont.collect_alphabet(acc);
            }
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
            // The End leaves that continue after a call/box are in its *return*
            // continuation `k`, so graft onto `k`; the callee/body terminal is the
            // pop point, not a composition seam, and is left untouched.
            GlobalType::GlobalCall {
                callee,
                subst,
                cont: k,
            } => GlobalType::GlobalCall {
                callee,
                subst,
                cont: Box::new(k.then(cont)),
            },
            GlobalType::GlobalBox {
                enter,
                body,
                exit,
                cont: k,
            } => GlobalType::GlobalBox {
                enter,
                body,
                exit,
                cont: Box::new(k.then(cont)),
            },
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

/// `call callee[subst] . cont` — an RSM call to a *named* sub-protocol, its roles
/// renamed by `subst` (callee-role → caller-role), continuing as `cont` on return.
pub fn gcall(callee: ProtocolRef, subst: BTreeMap<Role, Role>, cont: GlobalType) -> GlobalType {
    GlobalType::GlobalCall {
        callee,
        subst,
        cont: Box::new(cont),
    }
}

/// `box⟨enter⟩{ body }⟨exit⟩ . cont` — an inline hierarchical sub-region (an HSM
/// composite state realized as an RSM box).
pub fn gbox(enter: Label, body: GlobalType, exit: Label, cont: GlobalType) -> GlobalType {
    GlobalType::GlobalBox {
        enter,
        body: Box::new(body),
        exit,
        cont: Box::new(cont),
    }
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
