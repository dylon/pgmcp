//! Core CFSM vocabulary: roles, channels, message labels, and the latent/text
//! medium distinction (ADR-009).
//!
//! Only *communication* is an action — exactly what Multiparty Session Types
//! and the A2A wire capture. Everything here is plain data with no behaviour;
//! the transition relation lives in [`crate::csm::transition`].

use serde::{Deserialize, Serialize};

/// A protocol participant — e.g. `"Orchestrator"`, `"Reflector"`,
/// `"Tool-Caller"`. A newtype over `String` so role identity is explicit in
/// every signature (a bare `String` would be ambiguous against labels).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Role(pub String);

impl Role {
    pub fn new(s: impl Into<String>) -> Self {
        Role(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for Role {
    fn from(s: &str) -> Self {
        Role(s.to_string())
    }
}

impl From<String> for Role {
    fn from(s: String) -> Self {
        Role(s)
    }
}

/// The medium a message travels over (ADR-009, RecursiveMAS / Track B).
///
/// The protocol *skeleton* is identical for text MAS and latent RecursiveMAS;
/// only the medium differs. A black-box agent (Claude Code, Codex) has no
/// hidden-state access and can speak only [`MessageMedium::Text`]; a
/// [`MessageMedium::Latent`] edge requires a white-box backbone. The
/// projection side-condition that turns "black-box role on a latent edge" into
/// a `ProjectionError` is enforced in Phase R1; this enum is defined now so the
/// `Label` type is stable across both tracks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "medium", content = "data", rename_all = "snake_case")]
pub enum MessageMedium {
    /// Text / JSON over the A2A JSON-RPC wire. The default and the only medium a
    /// black-box agent can use.
    #[default]
    Text,
    /// A latent hidden-state tensor handed off via RecursiveLink. `hidden_size`
    /// pins the source backbone width; a receiving outer-link `W₃` maps it to
    /// the target width (or `W₃ = I` when the widths already match).
    Latent {
        hidden_size: usize,
        backbone_sig: String,
    },
}

impl MessageMedium {
    pub fn is_latent(&self) -> bool {
        matches!(self, MessageMedium::Latent { .. })
    }
}

/// A typed message label: the alphabet symbol exchanged on a channel. `name` is
/// the *selector* (branches are distinguished and matched by name); `medium` is
/// carried metadata that does not participate in branch selection.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Label {
    pub name: String,
    #[serde(default, skip_serializing_if = "MessageMedium::is_text")]
    pub medium: MessageMedium,
}

impl MessageMedium {
    /// Serde skip helper — keeps the common text case out of the JSON.
    fn is_text(&self) -> bool {
        matches!(self, MessageMedium::Text)
    }
}

impl Label {
    /// A text-medium label (the common case).
    pub fn text(name: impl Into<String>) -> Self {
        Label {
            name: name.into(),
            medium: MessageMedium::Text,
        }
    }

    /// A latent-medium label carrying a RecursiveLink hidden-state hand-off.
    pub fn latent(
        name: impl Into<String>,
        hidden_size: usize,
        backbone_sig: impl Into<String>,
    ) -> Self {
        Label {
            name: name.into(),
            medium: MessageMedium::Latent {
                hidden_size,
                backbone_sig: backbone_sig.into(),
            },
        }
    }

    pub fn is_latent(&self) -> bool {
        self.medium.is_latent()
    }
}

impl std::fmt::Display for Label {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}

/// A directed FIFO channel between two roles.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Channel {
    pub from: Role,
    pub to: Role,
}

impl Channel {
    pub fn new(from: Role, to: Role) -> Self {
        Channel { from, to }
    }
}

/// A communication action from the viewpoint of one role's local machine. A
/// `Send` emits a label toward `to`; a `Recv` consumes a label from `from`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", content = "data", rename_all = "snake_case")]
pub enum Action {
    Send { to: Role, label: Label },
    Recv { from: Role, label: Label },
}

impl Action {
    /// The label communicated by this action.
    pub fn label(&self) -> &Label {
        match self {
            Action::Send { label, .. } | Action::Recv { label, .. } => label,
        }
    }

    /// The peer role this action communicates with.
    pub fn peer(&self) -> &Role {
        match self {
            Action::Send { to, .. } => to,
            Action::Recv { from, .. } => from,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_label_omits_medium_in_json() {
        let l = Label::text("plan");
        let json = serde_json::to_string(&l).expect("serialize");
        assert_eq!(json, r#"{"name":"plan"}"#);
        let back: Label = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, l);
        assert!(!back.is_latent());
    }

    #[test]
    fn latent_label_round_trips_with_medium() {
        let l = Label::latent("thoughts", 4096, "qwen3-8b");
        let json = serde_json::to_string(&l).expect("serialize");
        let back: Label = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, l);
        assert!(back.is_latent());
    }

    #[test]
    fn action_accessors() {
        let a = Action::Send {
            to: Role::new("R"),
            label: Label::text("reflect_req"),
        };
        assert_eq!(a.peer().as_str(), "R");
        assert_eq!(a.label().name, "reflect_req");
    }
}
