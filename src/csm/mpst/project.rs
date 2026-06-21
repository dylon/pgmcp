//! Projection: derive a role's [`LocalType`] from a [`GlobalType`]
//! (`G ↾ role`), the Honda–Yoshida–Carbone construction restricted to the
//! merge actually needed by pgmcp's fixed patterns.
//!
//! The merge is **plain** (a role's behaviour is identical across the branches
//! of a choice it does not participate in) **plus** the standard *external
//! choice* merge: two receives from the *same* sender with distinct labels
//! combine into one [`LocalType::Branch`]. This is exactly what makes a choice's
//! *bystander* projectable — e.g. the Tool-Caller in Deliberation, who receives
//! `act_req` in one branch and `finish` in the other. Anything else surfaces
//! [`ProjectionError::Unmergeable`]; a branch is never silently picked.

use std::collections::BTreeMap;

use crate::csm::mpst::global::GlobalType;
use crate::csm::mpst::local::{LocalBranch, LocalType};
use crate::csm::role::Role;

/// Why a global type does not project onto some role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectionError {
    /// A bystander's branch continuations cannot be merged (they diverge in a
    /// way no later message can distinguish).
    Unmergeable { left: String, right: String },
    /// A `GlobalCall` names a sub-protocol absent from the compilation environment
    /// (so its box cannot be compiled). Well-formedness (`WF-CLOSED`) catches this
    /// earlier; it is surfaced here for callers that compile without pre-checking.
    UnresolvedCallee { name: String },
}

impl ProjectionError {
    pub fn message(&self) -> String {
        match self {
            ProjectionError::Unmergeable { left, right } => {
                format!("cannot merge projections: {left}  ⊓  {right}")
            }
            ProjectionError::UnresolvedCallee { name } => {
                format!("cannot compile call: sub-protocol '{name}' is not in the environment")
            }
        }
    }
}

/// Project `g` onto `role`, yielding that role's local type.
pub fn project(g: &GlobalType, role: &Role) -> Result<LocalType, ProjectionError> {
    match g {
        GlobalType::Interaction {
            from,
            to,
            label,
            cont,
        } => {
            let c = project(cont, role)?;
            if role == from {
                Ok(LocalType::Send {
                    to: to.clone(),
                    label: label.clone(),
                    cont: Box::new(c),
                })
            } else if role == to {
                Ok(LocalType::Recv {
                    from: from.clone(),
                    label: label.clone(),
                    cont: Box::new(c),
                })
            } else {
                // Bystander to this message: skip to the continuation.
                Ok(c)
            }
        }
        GlobalType::Choice { from, to, branches } => {
            if role == from {
                // This role selects (internal choice).
                let mut bs = Vec::with_capacity(branches.len());
                for b in branches {
                    bs.push(LocalBranch {
                        label: b.label.clone(),
                        cont: project(&b.cont, role)?,
                    });
                }
                Ok(LocalType::Select {
                    to: to.clone(),
                    branches: bs,
                })
            } else if role == to {
                // This role offers (external choice).
                let mut bs = Vec::with_capacity(branches.len());
                for b in branches {
                    bs.push(LocalBranch {
                        label: b.label.clone(),
                        cont: project(&b.cont, role)?,
                    });
                }
                Ok(LocalType::Branch {
                    from: from.clone(),
                    branches: bs,
                })
            } else {
                // Bystander: its behaviour must merge across all branches.
                let mut iter = branches.iter();
                let first = iter
                    .next()
                    .expect("well-formed choice has at least one branch");
                let mut acc = project(&first.cont, role)?;
                for b in iter {
                    let pj = project(&b.cont, role)?;
                    acc = merge(acc, pj)?;
                }
                Ok(acc)
            }
        }
        GlobalType::Rec { var, body } => Ok(LocalType::Rec {
            var: var.clone(),
            body: Box::new(project(body, role)?),
        }),
        GlobalType::GlobalCall {
            callee,
            subst,
            cont,
        } => {
            // PARTICIPATION RULE (the load-bearing reconciliation, ADR-030): a role
            // that plays in the call frame (it is in the substitution image) projects
            // to a `LocalCall` and pushes/pops synchronously with the other
            // participants; a *bystander* skips the entire closed frame and projects
            // only the return continuation. WF-THREAD guarantees the frame's role-set
            // is fixed across choice branches, so a bystander is never "in the call in
            // one branch and out in another" — keeping `merge` total and projection
            // sound without any global broadcast.
            let c = project(cont, role)?;
            if subst.values().any(|r| r == role) {
                Ok(LocalType::LocalCall {
                    callee: callee.clone(),
                    subst: subst.clone(),
                    cont: Box::new(c),
                })
            } else {
                Ok(c)
            }
        }
        GlobalType::GlobalBox {
            enter,
            body,
            exit,
            cont,
        } => {
            // Same participation rule for an inline box: a role appearing in the body
            // plays the box; a bystander skips it (projecting only the continuation).
            let c = project(cont, role)?;
            if body.participants().contains(role) {
                Ok(LocalType::LocalBox {
                    enter: enter.clone(),
                    body: Box::new(project(body, role)?),
                    exit: exit.clone(),
                    cont: Box::new(c),
                })
            } else {
                Ok(c)
            }
        }
        GlobalType::Var { var } => Ok(LocalType::Var { var: var.clone() }),
        GlobalType::End => Ok(LocalType::End),
    }
}

/// Merge two local types that must coincide at a choice point this role does
/// not drive. Identical types merge to themselves; receives/branches from the
/// same sender combine into one external choice; same-named recursions merge
/// their bodies. Everything else is [`ProjectionError::Unmergeable`].
pub fn merge(a: LocalType, b: LocalType) -> Result<LocalType, ProjectionError> {
    if a == b {
        return Ok(a);
    }

    // External-choice merge: both sides reduce to an offer from the SAME sender.
    if let (Some((fa, ba)), Some((fb, bb))) = (as_offer(&a), as_offer(&b))
        && fa == fb
    {
        return merge_offers(fa, ba, bb);
    }

    // Same-named recursion: merge the bodies.
    if let (LocalType::Rec { var: v1, body: b1 }, LocalType::Rec { var: v2, body: b2 }) = (&a, &b)
        && v1 == v2
    {
        let merged = merge((**b1).clone(), (**b2).clone())?;
        return Ok(LocalType::Rec {
            var: v1.clone(),
            body: Box::new(merged),
        });
    }

    // Same call (identical callee + role renaming): merge the return continuations.
    // A bystander that sees the *same* sub-protocol call in two choice branches but
    // with different post-return behaviour is projectable (WF-THREAD guarantees the
    // call's role-set, hence its participation, is identical across branches).
    if let (
        LocalType::LocalCall {
            callee: c1,
            subst: s1,
            cont: k1,
        },
        LocalType::LocalCall {
            callee: c2,
            subst: s2,
            cont: k2,
        },
    ) = (&a, &b)
        && c1 == c2
        && s1 == s2
    {
        let merged = merge((**k1).clone(), (**k2).clone())?;
        return Ok(LocalType::LocalCall {
            callee: c1.clone(),
            subst: s1.clone(),
            cont: Box::new(merged),
        });
    }

    // Same box (identical enter/exit boundary): merge body and continuation.
    if let (
        LocalType::LocalBox {
            enter: e1,
            body: bd1,
            exit: x1,
            cont: k1,
        },
        LocalType::LocalBox {
            enter: e2,
            body: bd2,
            exit: x2,
            cont: k2,
        },
    ) = (&a, &b)
        && e1 == e2
        && x1 == x2
    {
        let merged_body = merge((**bd1).clone(), (**bd2).clone())?;
        let merged_cont = merge((**k1).clone(), (**k2).clone())?;
        return Ok(LocalType::LocalBox {
            enter: e1.clone(),
            body: Box::new(merged_body),
            exit: x1.clone(),
            cont: Box::new(merged_cont),
        });
    }

    Err(ProjectionError::Unmergeable {
        left: describe(&a),
        right: describe(&b),
    })
}

/// View a `Recv` or `Branch` as `(sender, branch-list)`; other shapes are not
/// offers and cannot participate in the external-choice merge.
fn as_offer(t: &LocalType) -> Option<(Role, Vec<LocalBranch>)> {
    match t {
        LocalType::Recv { from, label, cont } => Some((
            from.clone(),
            vec![LocalBranch {
                label: label.clone(),
                cont: (**cont).clone(),
            }],
        )),
        LocalType::Branch { from, branches } => Some((from.clone(), branches.clone())),
        _ => None,
    }
}

/// Combine two branch-lists from the same sender. Shared label names must have
/// mergeable continuations; distinct labels are unioned. The result is a single
/// `Branch` (a 1-arm branch is the canonical form of a lone `Recv` after merge).
fn merge_offers(
    from: Role,
    ba: Vec<LocalBranch>,
    bb: Vec<LocalBranch>,
) -> Result<LocalType, ProjectionError> {
    let mut map: BTreeMap<String, LocalBranch> = BTreeMap::new();
    for br in ba {
        map.insert(br.label.name.clone(), br);
    }
    for br in bb {
        match map.remove(&br.label.name) {
            Some(existing) => {
                let merged_cont = merge(existing.cont, br.cont)?;
                map.insert(
                    br.label.name.clone(),
                    LocalBranch {
                        label: existing.label,
                        cont: merged_cont,
                    },
                );
            }
            None => {
                map.insert(br.label.name.clone(), br);
            }
        }
    }
    Ok(LocalType::Branch {
        from,
        branches: map.into_values().collect(),
    })
}

/// A short one-line shape description for `Unmergeable` diagnostics.
fn describe(t: &LocalType) -> String {
    match t {
        LocalType::Send { to, label, .. } => format!("!{to}⟨{label}⟩…"),
        LocalType::Recv { from, label, .. } => format!("?{from}⟨{label}⟩…"),
        LocalType::Select { to, .. } => format!("⊕{to}{{…}}"),
        LocalType::Branch { from, .. } => format!("&{from}{{…}}"),
        LocalType::Rec { var, .. } => format!("μ{var}.…"),
        LocalType::LocalCall { callee, .. } => format!("call {}…", callee.name),
        LocalType::LocalBox { enter, exit, .. } => format!("box⟨{enter}⟩{{…}}⟨{exit}⟩…"),
        LocalType::Var { var } => var.clone(),
        LocalType::End => "end".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csm::mpst::global::{choice, end, gbranch, interaction, rec, var};
    use crate::csm::role::Label;

    fn r(name: &str) -> Role {
        Role::new(name)
    }

    #[test]
    fn sequential_projection_is_send_then_end() {
        // O → P : plan . end
        let g = interaction("O", "P", Label::text("plan"), end());
        assert_eq!(
            project(&g, &r("O")).unwrap(),
            LocalType::send("P", Label::text("plan"), LocalType::End)
        );
        assert_eq!(
            project(&g, &r("P")).unwrap(),
            LocalType::recv("O", Label::text("plan"), LocalType::End)
        );
        // A bystander projects to End.
        assert_eq!(project(&g, &r("Z")).unwrap(), LocalType::End);
    }

    #[test]
    fn merge_combines_receives_from_same_sender() {
        // Two receives from O with distinct labels merge into a Branch offering
        // both — the heart of bystander projection over a choice.
        let a = LocalType::recv("O", Label::text("finish"), LocalType::End);
        let b = LocalType::recv("O", Label::text("act"), LocalType::var("t"));
        let merged = merge(a, b).expect("mergeable");
        match merged {
            LocalType::Branch { from, branches } => {
                assert_eq!(from, r("O"));
                assert_eq!(branches.len(), 2);
                let names: Vec<_> = branches.iter().map(|x| x.label.name.clone()).collect();
                assert!(names.contains(&"finish".to_string()));
                assert!(names.contains(&"act".to_string()));
            }
            other => panic!("expected Branch, got {other:?}"),
        }
    }

    #[test]
    fn merge_rejects_send_vs_recv() {
        let a = LocalType::send("O", Label::text("x"), LocalType::End);
        let b = LocalType::recv("O", Label::text("x"), LocalType::End);
        assert!(matches!(
            merge(a, b),
            Err(ProjectionError::Unmergeable { .. })
        ));
    }

    #[test]
    fn choice_projects_to_select_for_sender_branch_for_receiver() {
        // R → O { yes: end ; no: end }
        let g = choice(
            "R",
            "O",
            vec![
                gbranch(Label::text("yes"), end()),
                gbranch(Label::text("no"), end()),
            ],
        );
        match project(&g, &r("R")).unwrap() {
            LocalType::Select { to, branches } => {
                assert_eq!(to, r("O"));
                assert_eq!(branches.len(), 2);
            }
            other => panic!("R should Select, got {other:?}"),
        }
        match project(&g, &r("O")).unwrap() {
            LocalType::Branch { from, branches } => {
                assert_eq!(from, r("R"));
                assert_eq!(branches.len(), 2);
            }
            other => panic!("O should Branch, got {other:?}"),
        }
    }

    #[test]
    fn bystander_with_divergent_unmergeable_behaviour_is_rejected() {
        // R → O { a: T → O : x . end ; b: O → T : y . end }
        // T sends in one branch and receives in the other → Unmergeable.
        let g = choice(
            "R",
            "O",
            vec![
                gbranch(
                    Label::text("a"),
                    interaction("T", "O", Label::text("x"), end()),
                ),
                gbranch(
                    Label::text("b"),
                    interaction("O", "T", Label::text("y"), end()),
                ),
            ],
        );
        assert!(matches!(
            project(&g, &r("T")),
            Err(ProjectionError::Unmergeable { .. })
        ));
    }

    #[test]
    fn recursive_projection_threads_the_variable() {
        // μt. O → R : ping . t   projected onto R is  μt. ?O⟨ping⟩ . t
        let g = rec("t", interaction("O", "R", Label::text("ping"), var("t")));
        assert_eq!(
            project(&g, &r("R")).unwrap(),
            LocalType::rec(
                "t",
                LocalType::recv("O", Label::text("ping"), LocalType::var("t"))
            )
        );
    }
}
