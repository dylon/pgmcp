//! Parameters for the new topic-model application tools (ADR-029, item 14).

use serde::Deserialize;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CrossProjectTopicRedundancyParams {
    /// Minimum number of projects a topic must span to count as shared
    /// (default 2, floored at 2).
    #[serde(default)]
    pub min_projects: Option<i64>,
    /// Max rows to return (default 50, max 500).
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkItemTopicsParams {
    /// Number of themes to cluster into (default 8, max 50).
    #[serde(default)]
    pub k: Option<i64>,
    /// Optional work-item kind filter (e.g. "bug").
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CommitTopicsParams {
    /// Number of themes to cluster into (default 8, max 50).
    #[serde(default)]
    pub k: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PromptTopicsParams {
    /// Number of themes to cluster into (default 8, max 50).
    #[serde(default)]
    pub k: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TopicScopedSearchParams {
    /// Semantic query.
    pub query: String,
    /// Restrict to this topic id (or use topic_label).
    #[serde(default)]
    pub topic_id: Option<i64>,
    /// Restrict to the best-matching topic by label (or use topic_id).
    #[serde(default)]
    pub topic_label: Option<String>,
    /// Max results (default 20, max 100).
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TopicQualityForecastParams {
    /// Project name to forecast (defaults to the global/workspace history).
    #[serde(default)]
    pub project: Option<String>,
    /// Quality-score threshold to forecast the crossing of (default 0.6).
    #[serde(default)]
    pub threshold: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DocCodeTopicAlignmentParams {
    /// Max per-topic rows to return (default 100, max 1000).
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TopicExperimentMapParams {
    /// Max topic rows to return (default 100, max 1000).
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TopicDriftWarningParams {
    /// Minimum |percent change| in chunk count (first→last snapshot) to flag as
    /// drift (default 0.5 = 50%).
    #[serde(default)]
    pub min_pct_change: Option<f64>,
    /// Max rows (default 50, max 500).
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TopicOwnershipForecastParams {
    /// Max topic rows to return (default 50, max 500).
    #[serde(default)]
    pub limit: Option<i64>,
}
