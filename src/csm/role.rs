//! Core CFSM vocabulary: roles, channels, message labels, and the latent/text
//! medium distinction (ADR-009).
//!
//! Only *communication* is an action — exactly what Multiparty Session Types
//! and the A2A wire capture. Everything here is plain data with no behaviour;
//! the transition relation lives in [`crate::csm::transition`].

use serde::{Deserialize, Serialize};

use crate::tracker::kind::join_quoted;

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

/// The visibly-pushdown stack action a protocol-alphabet symbol triggers
/// (Alur & Madhusudan, *Visibly Pushdown Languages*, STOC 2004): a *call* symbol
/// pushes a return context, a *return* symbol pops it, and an *internal* symbol
/// leaves the stack untouched. The action is determined by the **symbol class**,
/// not the state — this "visibility" is exactly what keeps conformance decidable
/// and the language class closed under ∩/∪/¬ (so compositional conformance is
/// decidable). It is what lifts the CSM from finite-state (regular) to a
/// *visibly pushdown* recognizer over well-nested protocol runs.
///
/// Stored in `csm_protocol_alphabet.stack_action` (the ADR-003 idiom: a `TEXT`
/// column plus a `CHECK` built from [`StackAction::sql_in_list`], with a
/// `#[cfg(test)]` golden test pinning the set — the same closed-vocabulary
/// discipline as `PageState` (`crate::tape::vocab`) and `SessionStatus`
/// (`crate::csm::session_store`)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StackAction {
    /// Σ_int — an ordinary `Interaction`/`Choice` message; the stack is unchanged.
    Neutral,
    /// Σ_call — entering a sub-protocol frame (a `GlobalCall`/`GlobalBox` boundary);
    /// push the matching return context.
    Push,
    /// Σ_ret — a sub-protocol reached `End`; pop the matching return context.
    Pop,
}

impl StackAction {
    /// Canonical ordering; also the source of the DB CHECK vocabulary.
    pub const ALL: &'static [StackAction] = &[Self::Neutral, Self::Push, Self::Pop];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Neutral => "neutral",
            Self::Push => "push",
            Self::Pop => "pop",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|x| x.as_str() == s)
    }

    /// SQL `IN (...)` value list — the single source of truth shared with the
    /// `csm_protocol_alphabet.stack_action` CHECK constraint (v54 migration).
    pub fn sql_in_list() -> String {
        join_quoted(Self::ALL.iter().map(|x| x.as_str()))
    }
}

/// The maximum pushdown stack depth — one shared bound across the whole CSM:
/// the type layer's well-formedness check (`WF-BOUND` in
/// [`crate::csm::mpst::wellformed`]), the conformance engine
/// (`PdaDecoder::with_max_stack_depth` in [`crate::csm::conformance`]), and the
/// RLM runtime ([`crate::a2a::rlm`]), so the conformance stack and the runtime
/// call stack are literally the same depth.
///
/// It equals `lling_llang::pushdown::PdaDecoder::DEFAULT_MAX_STACK_DEPTH` (a
/// `#[cfg(test)]` test in [`crate::csm::conformance`] pins the equality so the two
/// cannot drift). A *large finite* bound is the bridge that keeps the recognized
/// language decidable (Alur–Madhusudan), keeps the Rocq model an ordinary
/// `Inductive` (no coinduction — the reachable-configuration set stays finite),
/// and makes termination provable via a `(MAX_STACK_DEPTH − depth, …)` measure.
/// It is config-overridable at the runtime edge (`[a2a.rlm] max_depth`).
pub const MAX_STACK_DEPTH: usize = 4096;

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

    #[test]
    fn stack_action_vocabulary_is_pinned() {
        // ADR-003 golden test: the DB CHECK vocabulary must match this exact
        // closed set; if a variant is added/renamed, this fails until the v54
        // `csm_protocol_alphabet` CHECK is regenerated from `sql_in_list()`.
        assert_eq!(StackAction::sql_in_list(), "'neutral','push','pop'");
        for a in StackAction::ALL {
            assert_eq!(StackAction::parse(a.as_str()), Some(*a));
        }
        assert_eq!(StackAction::parse("bogus"), None);
    }

    #[test]
    fn stack_action_round_trips_as_snake_case_json() {
        for a in StackAction::ALL {
            let json = serde_json::to_string(a).expect("serialize");
            assert_eq!(json, format!("\"{}\"", a.as_str()));
            let back: StackAction = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, *a);
        }
    }
}
