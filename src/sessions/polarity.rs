//! Polarity / cue-tier / mandate-status enums — extracted from the parent
//! `sessions.rs` as part of the D.2 god-file split.

use serde::{Deserialize, Serialize};

/// 12 polarities derived from corpus mining of the user's prompt history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MandatePolarity {
    /// Universal positive directive (`regardless`, `every time`).
    Always,
    /// Universal negation with standing-scope gate (`never X again`).
    Never,
    /// Preference between alternatives (`prefer X over Y`, `instead of`).
    Prefer,
    /// Specific thing to dodge (`avoid`, `try not to`, `no need`).
    Avoid,
    /// Habitual reminder (`remember to`, `make sure`, `be sure`).
    Remember,
    /// Temporal scope (`anytime you`, `in the future`, `next time`).
    FromNowOn,
    /// Negative reprimand surfacing an existing rule (`I told you`, `again?!`).
    Correction,
    /// Approval-gated rule (`without my approval`, `unless I explicitly`).
    Permission,
    /// Structural impossibility (`cannot`, `not allowed`, `forbidden`).
    Constraint,
    /// Explicit rule flag (`must`, `non-negotiable`, `golden rule`).
    Mandate,
    /// Step-order rule (`always X before Y`).
    ProcessRule,
    /// Project-scoped rule (`in this project`, `for this repo`).
    ProjectRule,
}

impl MandatePolarity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::Never => "never",
            Self::Prefer => "prefer",
            Self::Avoid => "avoid",
            Self::Remember => "remember",
            Self::FromNowOn => "from_now_on",
            Self::Correction => "correction",
            Self::Permission => "permission",
            Self::Constraint => "constraint",
            Self::Mandate => "mandate",
            Self::ProcessRule => "process_rule",
            Self::ProjectRule => "project_rule",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "always" => Some(Self::Always),
            "never" => Some(Self::Never),
            "prefer" => Some(Self::Prefer),
            "avoid" => Some(Self::Avoid),
            "remember" => Some(Self::Remember),
            "from_now_on" => Some(Self::FromNowOn),
            "correction" => Some(Self::Correction),
            "permission" => Some(Self::Permission),
            "constraint" => Some(Self::Constraint),
            "mandate" => Some(Self::Mandate),
            "process_rule" => Some(Self::ProcessRule),
            "project_rule" => Some(Self::ProjectRule),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum CueTier {
    F,
    E,
    D,
    C,
    B,
    A,
}

impl CueTier {
    pub fn as_char(self) -> char {
        match self {
            Self::A => 'A',
            Self::B => 'B',
            Self::C => 'C',
            Self::D => 'D',
            Self::E => 'E',
            Self::F => 'F',
        }
    }
    /// Used by SessionMandate consumers that need to compare row tiers
    /// (e.g. the cron refinement pass). Not yet called from the bin path.
    #[allow(dead_code)]
    pub fn from_char(c: char) -> Self {
        match c {
            'A' => Self::A,
            'B' => Self::B,
            'C' => Self::C,
            'E' => Self::E,
            'F' => Self::F,
            _ => Self::D,
        }
    }
}

/// Status taxonomy persisted in `session_mandates.status`. The MCP / REST
/// surface treats the column as a string today; this enum is exposed for
/// callers that want strongly-typed status handling (cron refinement,
/// integration tests).
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MandateStatus {
    Active,
    Superseded,
    Retired,
    Promoted,
}

impl MandateStatus {
    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Superseded => "superseded",
            Self::Retired => "retired",
            Self::Promoted => "promoted",
        }
    }
}
