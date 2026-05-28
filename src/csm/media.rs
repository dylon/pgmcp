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
}
