//! Parameters for the category-theoretic tools (ADR-028, item 4).

use serde::Deserialize;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CategoricalLintParams {
    /// Re-aggregate the group + workspace rollup before checking (default false).
    #[serde(default)]
    pub rebuild: Option<bool>,
}
