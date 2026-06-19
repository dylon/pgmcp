//! Parameter structs for the agent-feedback and voting tools (ADR-023).
//!
//! `agent_id` fields are injected by the `#[tool]` wrapper from the MCP caller
//! identity when the client omits them (the same idiom as the work-item tools),
//! so a client cannot easily vote/submit as someone else without spoofing its
//! `clientInfo.name`.

use serde::Deserialize;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SubmitFeedbackParams {
    /// What kind of feedback: complaint | feature_request | praise | bug_report |
    /// question | suggestion.
    pub category: String,
    /// Sentiment: strongly_negative | negative | neutral | positive |
    /// strongly_positive.
    pub sentiment: String,
    /// The feedback text.
    pub body: String,
    /// Optional short subject/title.
    #[serde(default)]
    pub subject: Option<String>,
    /// Optional pgmcp tool name this feedback is about.
    #[serde(default)]
    pub about_tool: Option<String>,
    /// Optional project name for scope.
    #[serde(default)]
    pub project: Option<String>,
    /// Submitting agent (auto-filled from the MCP caller if omitted).
    #[serde(default)]
    pub agent_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListFeedbackParams {
    /// Filter by category.
    #[serde(default)]
    pub category: Option<String>,
    /// Filter by sentiment.
    #[serde(default)]
    pub sentiment: Option<String>,
    /// Filter by triage status (open | acknowledged | planned | resolved | declined).
    #[serde(default)]
    pub status: Option<String>,
    /// Filter by the tool the feedback is about.
    #[serde(default)]
    pub about_tool: Option<String>,
    /// Filter by project name.
    #[serde(default)]
    pub project: Option<String>,
    /// Max rows (default 50).
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchFeedbackParams {
    /// Search text.
    pub query: String,
    /// Search mode: "fts" (keyword), "semantic" (vector), or "hybrid" (both).
    /// Defaults to "hybrid".
    #[serde(default)]
    pub mode: Option<String>,
    /// Max rows (default 20).
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RespondFeedbackParams {
    /// Feedback id.
    pub id: i64,
    /// New status: acknowledged | planned | resolved | declined (or open).
    pub status: String,
    /// Optional response text.
    #[serde(default)]
    pub response: Option<String>,
    /// Responding agent (auto-filled from the MCP caller if omitted).
    #[serde(default)]
    pub agent_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PromoteFeedbackParams {
    /// Feedback id to promote into a tracked work-item.
    pub id: i64,
    /// Optional work-item title (defaults to the feedback subject/body).
    #[serde(default)]
    pub title: Option<String>,
    /// Promoting agent (auto-filled from the MCP caller if omitted).
    #[serde(default)]
    pub agent_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CastVoteParams {
    /// Target kind: work_item | feedback | bug | experiment.
    pub target_type: String,
    /// Numeric id of the target.
    pub target_id: i64,
    /// Vote direction: up | down.
    pub direction: String,
    /// Optional weight (> 0, default 1.0).
    #[serde(default)]
    pub weight: Option<f32>,
    /// Voting agent (auto-filled from the MCP caller if omitted). One vote per
    /// (target, agent) — re-voting updates the existing vote.
    #[serde(default)]
    pub agent_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RetractVoteParams {
    /// Target kind: work_item | feedback | bug | experiment.
    pub target_type: String,
    /// Numeric id of the target.
    pub target_id: i64,
    /// Voting agent (auto-filled from the MCP caller if omitted).
    #[serde(default)]
    pub agent_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TallyVotesParams {
    /// Target kind: work_item | feedback | bug | experiment.
    pub target_type: String,
    /// Numeric id of the target.
    pub target_id: i64,
}
