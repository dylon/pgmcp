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
