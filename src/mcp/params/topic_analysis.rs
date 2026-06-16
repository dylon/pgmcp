//! Parameter types for the `topic_analysis` portfolio-analytics tools.

use serde::Deserialize;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProjectTopicProfileParams {
    /// Project name to fingerprint; omit to profile ALL indexed projects
    /// (a comparison table sorted by specialization).
    #[schemars(description = "Project name; omit for an all-projects comparison")]
    pub project: Option<String>,
    /// Top topics to list per project (default 10).
    #[schemars(description = "Top topics to list per project (default 10)")]
    pub top_n: Option<usize>,
    #[schemars(
        description = "Output format: json (default) | markdown | org | latex | html | text"
    )]
    pub format: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TopicProjectMapParams {
    /// Minimum number of projects a theme must span to be listed (default 2).
    #[schemars(description = "Minimum projects a theme must span to be shown (default 2)")]
    pub min_breadth: Option<i32>,
    #[schemars(
        description = "Output format: json (default) | markdown | org | latex | html | text"
    )]
    pub format: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProjectTopicSimilarityParams {
    /// Comparison space: "centroid" (default; cosine over aggregated topic
    /// centroids) or "global_jsd" (Jensen–Shannon over global-theme distributions).
    #[schemars(description = "Method: centroid (default) | global_jsd")]
    pub method: Option<String>,
    /// Average-linkage clustering similarity threshold (default 0.85).
    #[schemars(description = "Clustering similarity threshold (default 0.85)")]
    pub threshold: Option<f64>,
    #[schemars(
        description = "Output format: json (default) | markdown | org | latex | html | text"
    )]
    pub format: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TopicTrendsParams {
    /// Scope to analyze: "global" (default) or "project:NAME".
    #[schemars(description = "Scope: global (default) or project:NAME")]
    pub scope: Option<String>,
    /// Mode: "longitudinal" (default; per-topic size history), "quality"
    /// (aggregate metric trajectory), or "chunk_age" (blame-date proxy).
    #[schemars(description = "Mode: longitudinal (default) | quality | chunk_age")]
    pub mode: Option<String>,
    /// For mode=chunk_age: days that count as "recent" (default 90).
    #[schemars(description = "chunk_age: recent-window days (default 90)")]
    pub recent_days: Option<i32>,
    #[schemars(
        description = "Output format: json (default) | markdown | org | latex | html | text"
    )]
    pub format: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TopicOwnersParams {
    /// Project name (required).
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Top authors to list per topic (default 5).
    #[schemars(description = "Top authors to list per topic (default 5)")]
    pub top_authors: Option<usize>,
    #[schemars(
        description = "Output format: json (default) | markdown | org | latex | html | text"
    )]
    pub format: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TopicCooccurrenceParams {
    /// Project name (required).
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Minimum shared-chunk weight for a topic-pair edge (default 2).
    #[schemars(description = "Minimum shared chunks for a topic-pair edge (default 2)")]
    pub min_weight: Option<i64>,
    #[schemars(
        description = "Output format: json (default) | markdown | org | latex | html | text"
    )]
    pub format: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TopicCoverageGapsParams {
    /// Project name; omit to scan ALL indexed projects.
    #[schemars(description = "Project name; omit to scan all projects")]
    pub project: Option<String>,
    /// Topics with fewer chunks than this are flagged thin (default 5).
    #[schemars(description = "Topics with fewer chunks than this are flagged thin (default 5)")]
    pub thin_threshold: Option<i64>,
    /// Topics with cohesion below this are flagged low-cohesion (default 0.2).
    #[schemars(description = "Cohesion below this flags a low-cohesion topic (default 0.2)")]
    pub low_sim: Option<f64>,
    #[schemars(
        description = "Output format: json (default) | markdown | org | latex | html | text"
    )]
    pub format: Option<String>,
}
