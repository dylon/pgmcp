//! Tier-1 medium discipline (ADR-009 Phase R1). RecursiveMAS rides on the same
//! protocol skeleton; only the channel [`MessageMedium`] differs (Text vs
//! Latent). A **black-box** role — an agent with no hidden-state access (Claude
//! Code, Codex) — can speak only Text; a `Latent` edge touching a black-box role
//! is a projection-time error ("you cannot put Claude in the latent loop"). This
//! is the static side-condition the projector enforces before any agent runs;
//! the Rocq companion `docs/formal/rocq/CsmMedium.v` proves the discipline, and
//! `docs/formal/tla/RmasRecursionLoop.tla` model-checks the decode invariant.
//!
//! [`MessageMedium`]: crate::csm::role::MessageMedium

use std::collections::BTreeSet;

use crate::csm::mpst::global::GlobalType;
use crate::csm::role::Role;

/// Why a protocol violates the medium discipline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaError {
    /// A latent-medium communication touches a black-box role.
    BlackBoxOnLatent { role: String, label: String },
}

impl MediaError {
    pub fn message(&self) -> String {
        match self {
            MediaError::BlackBoxOnLatent { role, label } => format!(
                "black-box role '{role}' is on the latent edge '{label}' — black-box agents \
                 (no hidden-state access) may only communicate on Text-medium edges"
            ),
        }
    }
}

/// Check that no `Latent`-medium edge touches a black-box role. Pure and total —
/// the projection-time gate that makes "black-box role in the latent loop" a
/// type error rather than a runtime failure.
pub fn check_media_discipline(
    g: &GlobalType,
    black_box: &BTreeSet<Role>,
) -> Result<(), MediaError> {
    match g {
        GlobalType::Interaction {
            from,
            to,
            label,
            cont,
        } => {
            if label.is_latent() && (black_box.contains(from) || black_box.contains(to)) {
                let role = if black_box.contains(from) { from } else { to };
                return Err(MediaError::BlackBoxOnLatent {
                    role: role.to_string(),
                    label: label.name.clone(),
                });
            }
            check_media_discipline(cont, black_box)
        }
        GlobalType::Choice { from, to, branches } => {
            for b in branches {
                if b.label.is_latent() && (black_box.contains(from) || black_box.contains(to)) {
                    let role = if black_box.contains(from) { from } else { to };
                    return Err(MediaError::BlackBoxOnLatent {
                        role: role.to_string(),
                        label: b.label.name.clone(),
                    });
                }
                check_media_discipline(&b.cont, black_box)?;
            }
            Ok(())
        }
        GlobalType::Rec { body, .. } => check_media_discipline(body, black_box),
        // The `call:`/`ret:` boundary symbols are Text control events (black-box
        // legal by construction); the callee's own edges are checked when the
        // callee protocol is validated. So only the return continuation is local.
        GlobalType::GlobalCall { cont, .. } => check_media_discipline(cont, black_box),
        // A box's `enter`/`exit` are Text boundary symbols; its inline body carries
        // the real (from,to,label) edges, so check the body and the continuation.
        GlobalType::GlobalBox { body, cont, .. } => {
            check_media_discipline(body, black_box)?;
            check_media_discipline(cont, black_box)
        }
        GlobalType::Var { .. } | GlobalType::End => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csm::mpst::global::{end, interaction};
    use crate::csm::role::Label;

    fn bb(roles: &[&str]) -> BTreeSet<Role> {
        roles.iter().map(|r| Role::new(*r)).collect()
    }

    #[test]
    fn all_text_protocol_passes_for_any_black_box_set() {
        // O → P : plan (Text) . P → O : ans (Text) . end
        let g = interaction(
            "O",
            "P",
            Label::text("plan"),
            interaction("P", "O", Label::text("ans"), end()),
        );
        assert!(check_media_discipline(&g, &bb(&["O", "P"])).is_ok());
    }

    #[test]
    fn black_box_role_on_latent_edge_is_rejected() {
        // O → P : thoughts (Latent) — with P black-box, this is illegal.
        let g = interaction("O", "P", Label::latent("thoughts", 4096, "qwen3-8b"), end());
        let err = check_media_discipline(&g, &bb(&["P"])).expect_err("black-box P on latent edge");
        assert!(matches!(err, MediaError::BlackBoxOnLatent { .. }));
    }

    #[test]
    fn latent_edge_between_white_box_roles_passes() {
        // Same latent edge, but neither O nor P is black-box ⇒ allowed.
        let g = interaction("O", "P", Label::latent("thoughts", 4096, "qwen3-8b"), end());
        assert!(check_media_discipline(&g, &bb(&["Claude", "Codex"])).is_ok());
    }

    #[test]
    fn tape_paging_is_black_box_legal_for_o_tape_claude_codex() {
        // Phase 6: the whole TapePaging protocol is all-Text, so the discipline
        // admits it for ANY black-box set — including its own roles O + Tape plus
        // the agent roles Claude + Codex. This is the property that lets a
        // black-box orchestrator DRIVE paging (vs. the white-box-only latent tier).
        use crate::csm::registry::{ProtocolId, ProtocolParams, global_of};
        let g = global_of(ProtocolId::TapePaging, &ProtocolParams::default());
        check_media_discipline(&g, &bb(&["O", "Tape", "Claude", "Codex"]))
            .expect("all-Text TapePaging is black-box-legal for every role");
    }

    #[test]
    fn tape_paging_with_one_latent_edge_and_black_box_tape_is_rejected() {
        // CONTRAST: the discipline still BITES. Take the real TapePaging global
        // type and flip exactly ONE edge (the `page_in_ack` Tape→O reply) to a
        // latent medium. With `Tape` black-box, that single latent edge touching a
        // black-box role is a projection-time error — proving the all-Text version
        // passes for a reason (the medium gate), not by vacuity.
        use crate::csm::mpst::global::GlobalType;
        use crate::csm::registry::{ProtocolId, ProtocolParams, global_of};

        // Recursively rewrite the `page_in_ack` label to latent.
        fn make_ack_latent(g: GlobalType) -> GlobalType {
            match g {
                GlobalType::Interaction {
                    from,
                    to,
                    label,
                    cont,
                } => {
                    let label = if label.name == "page_in_ack" {
                        Label::latent("page_in_ack", 4096, "qwen3-8b")
                    } else {
                        label
                    };
                    GlobalType::Interaction {
                        from,
                        to,
                        label,
                        cont: Box::new(make_ack_latent(*cont)),
                    }
                }
                GlobalType::Choice { from, to, branches } => GlobalType::Choice {
                    from,
                    to,
                    branches: branches
                        .into_iter()
                        .map(|b| {
                            crate::csm::mpst::global::gbranch(b.label, make_ack_latent(b.cont))
                        })
                        .collect(),
                },
                GlobalType::Rec { var, body } => GlobalType::Rec {
                    var,
                    body: Box::new(make_ack_latent(*body)),
                },
                other @ (GlobalType::Var { .. }
                | GlobalType::End
                | GlobalType::GlobalCall { .. }
                | GlobalType::GlobalBox { .. }) => other,
            }
        }

        let g = make_ack_latent(global_of(
            ProtocolId::TapePaging,
            &ProtocolParams::default(),
        ));
        // Sanity: the all-Text original passed for {Tape}; the flipped one must not.
        assert!(
            check_media_discipline(
                &global_of(ProtocolId::TapePaging, &ProtocolParams::default()),
                &bb(&["Tape"])
            )
            .is_ok(),
            "the unflipped protocol is black-box-legal for Tape"
        );
        let err = check_media_discipline(&g, &bb(&["Tape"]))
            .expect_err("a latent page_in_ack with black-box Tape must be rejected");
        match err {
            MediaError::BlackBoxOnLatent { role, label } => {
                assert_eq!(role, "Tape");
                assert_eq!(label, "page_in_ack");
            }
        }
    }
}
