//! Well-formedness of a [`GlobalType`]: the static side-conditions that make a
//! protocol projectable and its projection meaningful.
//!
//! Checked (a pure, total walk):
//! 1. **No self-messages** — `from ≠ to` in every `Interaction`/`Choice`.
//! 2. **Closed** — every `Var` is bound by an enclosing `Rec`.
//! 3. **Guarded recursion** — a recursion variable never occurs at the head of
//!    its `Rec` body without a communication prefix (rejects `μt.t`, `μt.μs.t`).
//! 4. **Sender-driven choice** — non-empty branches with distinct label names.
//!
//! These are exactly the conditions the Rocq `wf` predicate will mechanize in
//! Phase 5; keeping the Rust check structurally parallel is intentional.

use std::collections::{BTreeMap, BTreeSet};

use crate::csm::mpst::global::{GlobalType, ProtocolEnv, TypeVar};
use crate::csm::role::{MAX_STACK_DEPTH, Role, StackAction};

/// Why a global type is not well-formed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WfError {
    /// An `Interaction` or `Choice` has `from == to`.
    SelfMessage { role: Role },
    /// A `Var` references an unbound recursion variable.
    UnboundVar { var: TypeVar },
    /// A `Rec` body exposes its own variable without a communication prefix.
    UnguardedRec { var: TypeVar },
    /// A `Choice` has no branches.
    EmptyChoice { from: Role, to: Role },
    /// Two `Choice` branches share a label name.
    DuplicateChoiceLabel { label: String },
    /// WF-CLOSED: a `GlobalCall` names a sub-protocol absent from the environment
    /// (so its frame could never be entered or projected).
    UnknownCallee { name: String },
    /// Matching-return: a `GlobalBox` has an empty boundary label, or a body that
    /// can never reach `End` — so the frame could never pop / return.
    MalformedBox { reason: String },
    /// WF-THREAD: a `GlobalCall`'s role renaming maps two callee roles onto the
    /// same caller role (it must be injective so the frame's participants stay
    /// distinct), or the call has no participants.
    NonInjectiveSubst { role: String },
    /// WF-VPA: one label name is used with two different stack actions — a
    /// violation of the visibly-pushdown discipline (the symbol class, not the
    /// state, must determine the stack action).
    VpaSymbolConflict { label: String },
    /// WF-VPA: an ordinary (`Neutral`) message label uses a reserved `call:`/`ret:`
    /// boundary prefix, which would collide with a synthesized call/return symbol.
    ReservedBoundaryLabel { label: String },
    /// WF-BOUND: static (acyclic) call nesting exceeds [`MAX_STACK_DEPTH`].
    StackBoundExceeded { depth: usize },
}

impl WfError {
    pub fn message(&self) -> String {
        match self {
            WfError::SelfMessage { role } => {
                format!("self-message: role '{role}' cannot send to itself")
            }
            WfError::UnboundVar { var } => format!("unbound recursion variable '{var}'"),
            WfError::UnguardedRec { var } => {
                format!(
                    "unguarded recursion: 'μ {var}. …' exposes '{var}' with no communication prefix"
                )
            }
            WfError::EmptyChoice { from, to } => {
                format!("empty choice '{from} → {to} {{}}'")
            }
            WfError::DuplicateChoiceLabel { label } => {
                format!("duplicate choice label '{label}'")
            }
            WfError::UnknownCallee { name } => {
                format!("call to unknown sub-protocol '{name}' (not in the protocol environment)")
            }
            WfError::MalformedBox { reason } => format!("malformed box: {reason}"),
            WfError::NonInjectiveSubst { role } => format!(
                "non-injective call substitution: two callee roles map onto caller role '{role}' \
                 (a call's role renaming must be injective)"
            ),
            WfError::VpaSymbolConflict { label } => format!(
                "visibly-pushdown violation: label '{label}' is used with two different stack \
                 actions (the symbol must determine push/pop/neutral)"
            ),
            WfError::ReservedBoundaryLabel { label } => {
                format!("label '{label}' uses the reserved 'call:'/'ret:' boundary prefix")
            }
            WfError::StackBoundExceeded { depth } => {
                format!("static call nesting {depth} exceeds MAX_STACK_DEPTH={MAX_STACK_DEPTH}")
            }
        }
    }
}

/// Check a global type for well-formedness against an *empty* environment — the
/// backward-compatible entry point for call-free protocols. A protocol that
/// contains a [`GlobalType::GlobalCall`] must use [`well_formed_in`] with the
/// environment that defines its callees (an empty env makes every call an
/// `UnknownCallee`).
pub fn well_formed(g: &GlobalType) -> Result<(), WfError> {
    well_formed_in(g, &ProtocolEnv::new())
}

/// Check a global type for well-formedness against `env` (which must define every
/// sub-protocol a `GlobalCall` names). Three passes:
/// 1. **structural** ([`check`]): no self-message, closed + guarded recursion,
///    sender-driven choice, well-formed boxes (matching-return), injective call
///    substitutions (WF-THREAD);
/// 2. **visibly-pushdown** ([`check_visibility`], WF-VPA): each label name has one
///    stack action and no message label squats the reserved `call:`/`ret:` prefix;
/// 3. **callees** ([`check_callees`], WF-CLOSED + WF-BOUND): every call resolves,
///    each callee is itself well-formed (cycle-guarded for recursion), and static
///    acyclic nesting stays within [`MAX_STACK_DEPTH`].
pub fn well_formed_in(g: &GlobalType, env: &ProtocolEnv) -> Result<(), WfError> {
    check(g, &mut Vec::new())?;
    check_visibility(g)?;
    check_callees(g, env, &mut BTreeSet::new(), 0)?;
    Ok(())
}

fn check(g: &GlobalType, bound: &mut Vec<TypeVar>) -> Result<(), WfError> {
    match g {
        GlobalType::Interaction { from, to, cont, .. } => {
            if from == to {
                return Err(WfError::SelfMessage { role: from.clone() });
            }
            check(cont, bound)
        }
        GlobalType::Choice { from, to, branches } => {
            if from == to {
                return Err(WfError::SelfMessage { role: from.clone() });
            }
            if branches.is_empty() {
                return Err(WfError::EmptyChoice {
                    from: from.clone(),
                    to: to.clone(),
                });
            }
            let mut seen: BTreeSet<&str> = BTreeSet::new();
            for b in branches {
                if !seen.insert(b.label.name.as_str()) {
                    return Err(WfError::DuplicateChoiceLabel {
                        label: b.label.name.clone(),
                    });
                }
            }
            for b in branches {
                check(&b.cont, bound)?;
            }
            Ok(())
        }
        GlobalType::Rec { var, body } => {
            if !guarded(body, var) {
                return Err(WfError::UnguardedRec { var: var.clone() });
            }
            bound.push(var.clone());
            let r = check(body, bound);
            bound.pop();
            r
        }
        GlobalType::Var { var } => {
            if bound.iter().any(|v| v == var) {
                Ok(())
            } else {
                Err(WfError::UnboundVar { var: var.clone() })
            }
        }
        GlobalType::GlobalCall { subst, cont, .. } => {
            // WF-THREAD (injectivity): a call must have ≥1 participant and no two
            // callee roles may collapse onto the same caller role.
            if subst.is_empty() {
                return Err(WfError::NonInjectiveSubst {
                    role: "<empty substitution>".to_string(),
                });
            }
            let mut seen: BTreeSet<&str> = BTreeSet::new();
            for r in subst.values() {
                if !seen.insert(r.as_str()) {
                    return Err(WfError::NonInjectiveSubst {
                        role: r.to_string(),
                    });
                }
            }
            check(cont, bound)
        }
        GlobalType::GlobalBox {
            enter,
            body,
            exit,
            cont,
        } => {
            // Matching-return: non-empty boundary labels + a body that CAN reach
            // `End` (otherwise the frame could never pop).
            if enter.name.is_empty() || exit.name.is_empty() {
                return Err(WfError::MalformedBox {
                    reason: "empty enter/exit boundary label".to_string(),
                });
            }
            if !can_terminate(body) {
                return Err(WfError::MalformedBox {
                    reason: format!(
                        "box body '{}' can never reach End (cannot return)",
                        enter.name
                    ),
                });
            }
            // The box body is a self-contained sub-region: its recursion variables
            // are scoped to the body (a `Var` may not escape the box to an outer
            // `Rec`), so check it under a fresh binder stack.
            check(body, &mut Vec::new())?;
            check(cont, bound)
        }
        GlobalType::End => Ok(()),
    }
}

/// `true` iff `var` does not occur at the head of `body` without a
/// communication prefix. A communication (`Interaction`/`Choice`) guards
/// everything below it; a same-named inner `Rec` shadows the variable.
fn guarded(body: &GlobalType, var: &str) -> bool {
    match body {
        GlobalType::Var { var: v } => v != var,
        // A communication, a call, or entering a box are all guarding actions
        // (real progress before the variable can recur).
        GlobalType::Interaction { .. }
        | GlobalType::Choice { .. }
        | GlobalType::GlobalCall { .. }
        | GlobalType::GlobalBox { .. }
        | GlobalType::End => true,
        GlobalType::Rec { var: inner, body } => {
            if inner == var {
                true // inner rec shadows `var`
            } else {
                guarded(body, var)
            }
        }
    }
}

/// `true` iff some execution path of `g` reaches `End` (used to reject a box body
/// that could never pop / return). Following a `Var` back-edge is a loop, not
/// termination.
fn can_terminate(g: &GlobalType) -> bool {
    match g {
        GlobalType::End => true,
        GlobalType::Var { .. } => false,
        GlobalType::Interaction { cont, .. } => can_terminate(cont),
        GlobalType::Choice { branches, .. } => branches.iter().any(|b| can_terminate(&b.cont)),
        GlobalType::Rec { body, .. } => can_terminate(body),
        // After a call/box returns, termination is decided by the continuation (the
        // callee/body's own return is guaranteed by its own well-formedness).
        GlobalType::GlobalCall { cont, .. } => can_terminate(cont),
        GlobalType::GlobalBox { cont, .. } => can_terminate(cont),
    }
}

/// WF-VPA: the protocol's alphabet must assign each label name a SINGLE stack
/// action (the visibly-pushdown discipline — the symbol class, not the state,
/// determines push/pop/neutral), and no ordinary (`Neutral`) message label may
/// squat the reserved `call:`/`ret:` boundary prefix.
fn check_visibility(g: &GlobalType) -> Result<(), WfError> {
    let mut action_of: BTreeMap<String, StackAction> = BTreeMap::new();
    for (label, action) in g.alphabet() {
        if action == StackAction::Neutral
            && (label.starts_with("call:") || label.starts_with("ret:"))
        {
            return Err(WfError::ReservedBoundaryLabel { label });
        }
        match action_of.get(&label) {
            Some(prev) if *prev != action => return Err(WfError::VpaSymbolConflict { label }),
            _ => {
                action_of.insert(label, action);
            }
        }
    }
    Ok(())
}

/// WF-CLOSED + WF-BOUND: every `GlobalCall` resolves in `env`; each resolved
/// callee is itself structurally well-formed and visibly-pushdown (checked once,
/// cycle-guarded so self-/mutual recursion terminates the walk); and static
/// acyclic call nesting stays within [`MAX_STACK_DEPTH`].
fn check_callees(
    g: &GlobalType,
    env: &ProtocolEnv,
    visiting: &mut BTreeSet<String>,
    depth: usize,
) -> Result<(), WfError> {
    if depth > MAX_STACK_DEPTH {
        return Err(WfError::StackBoundExceeded { depth });
    }
    match g {
        GlobalType::Interaction { cont, .. } => check_callees(cont, env, visiting, depth),
        GlobalType::Choice { branches, .. } => {
            for b in branches {
                check_callees(&b.cont, env, visiting, depth)?;
            }
            Ok(())
        }
        GlobalType::Rec { body, .. } => check_callees(body, env, visiting, depth),
        GlobalType::GlobalBox { body, cont, .. } => {
            check_callees(body, env, visiting, depth + 1)?;
            check_callees(cont, env, visiting, depth)
        }
        GlobalType::GlobalCall { callee, cont, .. } => {
            match env.resolve(callee) {
                None => {
                    return Err(WfError::UnknownCallee {
                        name: callee.name.clone(),
                    });
                }
                Some(body) => {
                    // Cycle-guard: descend into a callee only if it is not already
                    // being expanded on this path, so recursion terminates the walk.
                    if visiting.insert(callee.name.clone()) {
                        check(body, &mut Vec::new())?;
                        check_visibility(body)?;
                        check_callees(body, env, visiting, depth + 1)?;
                        visiting.remove(&callee.name);
                    }
                }
            }
            check_callees(cont, env, visiting, depth)
        }
        GlobalType::Var { .. } | GlobalType::End => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csm::mpst::global::{choice, end, gbranch, interaction, rec, var};
    use crate::csm::role::Label;

    #[test]
    fn good_protocol_is_well_formed() {
        let g = interaction(
            "O",
            "R",
            Label::text("q"),
            interaction("R", "O", Label::text("a"), end()),
        );
        assert!(well_formed(&g).is_ok());
    }

    #[test]
    fn self_message_rejected() {
        let g = interaction("O", "O", Label::text("loop"), end());
        assert_eq!(
            well_formed(&g),
            Err(WfError::SelfMessage {
                role: Role::new("O")
            })
        );
    }

    #[test]
    fn unbound_var_rejected() {
        let g = interaction("O", "R", Label::text("q"), var("t"));
        assert_eq!(
            well_formed(&g),
            Err(WfError::UnboundVar {
                var: "t".to_string()
            })
        );
    }

    #[test]
    fn unguarded_recursion_rejected() {
        // μt. t
        assert_eq!(
            well_formed(&rec("t", var("t"))),
            Err(WfError::UnguardedRec {
                var: "t".to_string()
            })
        );
        // μt. μs. t  (t is unguarded across the inner binder)
        assert_eq!(
            well_formed(&rec("t", rec("s", var("t")))),
            Err(WfError::UnguardedRec {
                var: "t".to_string()
            })
        );
    }

    #[test]
    fn guarded_recursion_accepted() {
        // μt. O → R : l . t
        let g = rec("t", interaction("O", "R", Label::text("l"), var("t")));
        assert!(well_formed(&g).is_ok());
    }

    #[test]
    fn duplicate_choice_label_rejected() {
        let g = choice(
            "R",
            "O",
            vec![
                gbranch(Label::text("x"), end()),
                gbranch(Label::text("x"), end()),
            ],
        );
        assert_eq!(
            well_formed(&g),
            Err(WfError::DuplicateChoiceLabel {
                label: "x".to_string()
            })
        );
    }
}
