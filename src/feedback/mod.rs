//! Agent feedback over the MCP API — what connecting agents/clients like,
//! dislike, and want feature-wise about pgmcp itself.
//!
//! A standalone, agent-voice channel (distinct from the work-item tracker, which
//! is project work): an agent submits a categorized, sentiment-tagged note via
//! the `submit_feedback` tool; the corpus is queryable (`list_feedback` /
//! `search_feedback`), triageable (`respond_feedback`), and promotable into a
//! tracked work-item (`promote_feedback_to_work_item`). Rows are embedded on
//! write for semantic recall, exactly like `work_items`.
//!
//! Closed vocabularies follow the ADR-003 idiom (TEXT column + CHECK built from
//! `sql_in_list()` + a golden parity test). Schema: v43 (`agent_feedback`).

use serde::{Deserialize, Serialize};

/// Build a SQL `'a', 'b'` list from string slices for a CHECK constraint.
fn quoted_list<'a>(items: impl Iterator<Item = &'a str>) -> String {
    items
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// What kind of feedback this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackCategory {
    Complaint,
    FeatureRequest,
    Praise,
    BugReport,
    Question,
    Suggestion,
}

impl FeedbackCategory {
    pub const ALL: &'static [FeedbackCategory] = &[
        Self::Complaint,
        Self::FeatureRequest,
        Self::Praise,
        Self::BugReport,
        Self::Question,
        Self::Suggestion,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complaint => "complaint",
            Self::FeatureRequest => "feature_request",
            Self::Praise => "praise",
            Self::BugReport => "bug_report",
            Self::Question => "question",
            Self::Suggestion => "suggestion",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|x| x.as_str() == s)
    }

    pub fn sql_in_list() -> String {
        quoted_list(Self::ALL.iter().map(|x| x.as_str()))
    }
}

/// A five-point sentiment scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackSentiment {
    StronglyNegative,
    Negative,
    Neutral,
    Positive,
    StronglyPositive,
}

impl FeedbackSentiment {
    pub const ALL: &'static [FeedbackSentiment] = &[
        Self::StronglyNegative,
        Self::Negative,
        Self::Neutral,
        Self::Positive,
        Self::StronglyPositive,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::StronglyNegative => "strongly_negative",
            Self::Negative => "negative",
            Self::Neutral => "neutral",
            Self::Positive => "positive",
            Self::StronglyPositive => "strongly_positive",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|x| x.as_str() == s)
    }

    pub fn sql_in_list() -> String {
        quoted_list(Self::ALL.iter().map(|x| x.as_str()))
    }
}

/// Triage lifecycle of a feedback item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackStatus {
    Open,
    Acknowledged,
    Planned,
    Resolved,
    Declined,
}

impl FeedbackStatus {
    pub const ALL: &'static [FeedbackStatus] = &[
        Self::Open,
        Self::Acknowledged,
        Self::Planned,
        Self::Resolved,
        Self::Declined,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Acknowledged => "acknowledged",
            Self::Planned => "planned",
            Self::Resolved => "resolved",
            Self::Declined => "declined",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|x| x.as_str() == s)
    }

    pub fn sql_in_list() -> String {
        quoted_list(Self::ALL.iter().map(|x| x.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_roundtrips_and_quotes() {
        for c in FeedbackCategory::ALL {
            assert_eq!(FeedbackCategory::parse(c.as_str()), Some(*c));
        }
        assert_eq!(FeedbackCategory::ALL.len(), 6);
        assert!(FeedbackCategory::sql_in_list().contains("'feature_request'"));
        assert_eq!(FeedbackCategory::parse("nope"), None);
    }

    #[test]
    fn sentiment_roundtrips_and_quotes() {
        for s in FeedbackSentiment::ALL {
            assert_eq!(FeedbackSentiment::parse(s.as_str()), Some(*s));
        }
        assert_eq!(FeedbackSentiment::ALL.len(), 5);
        assert!(FeedbackSentiment::sql_in_list().contains("'strongly_negative'"));
    }

    #[test]
    fn status_roundtrips_and_quotes() {
        for s in FeedbackStatus::ALL {
            assert_eq!(FeedbackStatus::parse(s.as_str()), Some(*s));
        }
        assert_eq!(FeedbackStatus::ALL.len(), 5);
        assert!(FeedbackStatus::sql_in_list().contains("'acknowledged'"));
    }
}
