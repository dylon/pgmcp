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

use std::collections::BTreeSet;

use crate::csm::mpst::global::{GlobalType, TypeVar};
use crate::csm::role::Role;

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
        }
    }
}

/// Check a global type for well-formedness.
pub fn well_formed(g: &GlobalType) -> Result<(), WfError> {
    check(g, &mut Vec::new())
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
        GlobalType::End => Ok(()),
    }
}

/// `true` iff `var` does not occur at the head of `body` without a
/// communication prefix. A communication (`Interaction`/`Choice`) guards
/// everything below it; a same-named inner `Rec` shadows the variable.
fn guarded(body: &GlobalType, var: &str) -> bool {
    match body {
        GlobalType::Var { var: v } => v != var,
        GlobalType::Interaction { .. } | GlobalType::Choice { .. } | GlobalType::End => true,
        GlobalType::Rec { var: inner, body } => {
            if inner == var {
                true // inner rec shadows `var`
            } else {
                guarded(body, var)
            }
        }
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
