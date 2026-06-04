//! Closed vocabularies for the A2A agent mailbox (Phase 3) and the Phase-4
//! `WorktreeNegotiation` protocol that rides it. Per ADR-003 each is a `TEXT`
//! column + a `CHECK` built from a closed Rust enum via a `sql_in_list` helper,
//! with a `#[cfg(test)]` golden test pinning the set — the same idiom as
//! [`crate::tracker::severity`].

use serde::{Deserialize, Serialize};

use crate::tracker::kind::join_quoted;

/// The kind of an `agent_messages` row. The first three are general mailbox
/// envelopes; the last four are the typed steps of the Phase-4
/// `WorktreeNegotiation` protocol (request → accept|decline → moved). All are
/// pinned in the v27 `kind` CHECK up front so Phase 4 needs no schema change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    /// A plain informational message.
    Message,
    /// A request that invites a reply / action.
    Request,
    /// A fire-and-forget heads-up ("for your information").
    Fyi,
    /// Phase-4: "please move your in-flight edits to a worktree, restore stable".
    RequestWorktree,
    /// Phase-4: the editor accepts a `request_worktree`.
    Accept,
    /// Phase-4: the editor declines a `request_worktree` (with a reason in body).
    Decline,
    /// Phase-4: the editor reports the worktree move is done (candidate; the git
    /// scanner is the gatekeeper that actually unblocks the dependent).
    Moved,
}

impl MessageKind {
    /// Canonical set; the source of the DB CHECK vocabulary.
    pub const ALL: &'static [MessageKind] = &[
        Self::Message,
        Self::Request,
        Self::Fyi,
        Self::RequestWorktree,
        Self::Accept,
        Self::Decline,
        Self::Moved,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Request => "request",
            Self::Fyi => "fyi",
            Self::RequestWorktree => "request_worktree",
            Self::Accept => "accept",
            Self::Decline => "decline",
            Self::Moved => "moved",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.as_str() == s)
    }
}

/// SQL `IN (...)` value list for the `agent_messages_kind_check` constraint.
pub fn kind_sql_in_list() -> String {
    join_quoted(MessageKind::ALL.iter().map(|k| k.as_str()))
}

/// Which channel delivered a message to a recipient — stored in
/// `agent_message_receipts.channel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryChannel {
    /// UserPromptSubmit `additional_context` (next-turn).
    Prompt,
    /// SessionStart `pgmcp context` (next-turn / session start).
    SessionStart,
    /// PostToolUse hook `additionalContext` (mid-agentic-loop).
    Posttooluse,
    /// The recipient's own `a2a_inbox` pull (the reliable floor).
    InboxPull,
}

impl DeliveryChannel {
    /// Canonical set; the source of the DB CHECK vocabulary.
    pub const ALL: &'static [DeliveryChannel] = &[
        Self::Prompt,
        Self::SessionStart,
        Self::Posttooluse,
        Self::InboxPull,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prompt => "prompt",
            Self::SessionStart => "session_start",
            Self::Posttooluse => "posttooluse",
            Self::InboxPull => "inbox_pull",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|c| c.as_str() == s)
    }
}

/// SQL `IN (...)` value list for the `agent_message_receipts_channel_check`.
pub fn channel_sql_in_list() -> String {
    join_quoted(DeliveryChannel::ALL.iter().map(|c| c.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn message_kind_vocabulary_is_pinned() {
        let got: HashSet<&str> = MessageKind::ALL.iter().map(|k| k.as_str()).collect();
        let expected: HashSet<&str> = [
            "message",
            "request",
            "fyi",
            "request_worktree",
            "accept",
            "decline",
            "moved",
        ]
        .into_iter()
        .collect();
        assert_eq!(
            got, expected,
            "MessageKind vocabulary drifted from pinned set"
        );
        assert_eq!(MessageKind::ALL.len(), 7);
        assert_eq!(got.len(), 7, "duplicate as_str() in MessageKind");
    }

    #[test]
    fn delivery_channel_vocabulary_is_pinned() {
        let got: HashSet<&str> = DeliveryChannel::ALL.iter().map(|c| c.as_str()).collect();
        let expected: HashSet<&str> = ["prompt", "session_start", "posttooluse", "inbox_pull"]
            .into_iter()
            .collect();
        assert_eq!(got, expected, "DeliveryChannel vocabulary drifted");
        assert_eq!(DeliveryChannel::ALL.len(), 4);
        assert_eq!(got.len(), 4, "duplicate as_str() in DeliveryChannel");
    }

    #[test]
    fn parse_roundtrips_for_all() {
        for k in MessageKind::ALL {
            assert_eq!(MessageKind::parse(k.as_str()), Some(*k));
        }
        assert_eq!(MessageKind::parse("nope"), None);
        for c in DeliveryChannel::ALL {
            assert_eq!(DeliveryChannel::parse(c.as_str()), Some(*c));
        }
        assert_eq!(DeliveryChannel::parse("nope"), None);
    }

    #[test]
    fn sql_in_lists_quote_every_value() {
        let k = kind_sql_in_list();
        assert!(k.contains("'request_worktree'"), "got: {k}");
        assert_eq!(k.matches('\'').count(), MessageKind::ALL.len() * 2);
        assert_eq!(k.matches(',').count(), MessageKind::ALL.len() - 1);
        let c = channel_sql_in_list();
        assert!(c.contains("'inbox_pull'"), "got: {c}");
        assert_eq!(c.matches('\'').count(), DeliveryChannel::ALL.len() * 2);
        assert_eq!(c.matches(',').count(), DeliveryChannel::ALL.len() - 1);
    }
}
