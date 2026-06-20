//! Parameters for the category-theoretic tools (ADR-028, item 4).

use serde::Deserialize;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CategoricalLintParams {
    /// Re-aggregate the group + workspace rollup before checking (default false).
    #[serde(default)]
    pub rebuild: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CommonDependencyParams {
    /// First project (name or id).
    pub project_a: String,
    /// Second project (name or id).
    pub project_b: String,
    /// Max rows (default 100, max 1000).
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct IntegrationPointParams {
    /// First project (name or id).
    pub project_a: String,
    /// Second project (name or id).
    pub project_b: String,
    /// Max rows (default 100, max 1000).
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FunctorialImpactParams {
    /// Max rows (default 50, max 500).
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EffectFunctorParams {
    /// Project name or id.
    pub project: String,
    /// Max symbol rows (default 50, max 500).
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NaturalityGapParams {
    /// Project name or id.
    pub project: String,
    /// Similarity threshold below which an import edge is a "gap" (default 0.5).
    #[serde(default)]
    pub threshold: Option<f64>,
    /// Max rows (default 50, max 500).
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ColimitViewParams {
    /// Max component rows (default 100, max 1000).
    #[serde(default)]
    pub limit: Option<i64>,
}
