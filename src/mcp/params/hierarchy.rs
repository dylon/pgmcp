//! Parameters for the hierarchical inter-project tools (ADR-027).

use serde::Deserialize;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProjectGroupsParams {
    /// Re-derive worktree-family / singleton groups from git metadata before
    /// listing (default true). Set false to read the stored grouping as-is.
    #[serde(default)]
    pub rederive: Option<bool>,
    /// Optional `GroupKind` filter (worktree_family | monorepo | declared | manual).
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkspaceArchitectureQualityParams {
    /// Re-aggregate the group + workspace levels from existing `project_metrics`
    /// before reading (default false). The per-project level is filled by the
    /// graph-analysis cron, not here.
    #[serde(default)]
    pub rebuild: Option<bool>,
    /// Max group/project rows to return (default 100, max 1000).
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CrossProjectCouplingParams {
    /// Max project rows to return (default 100, max 1000).
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CrossProjectCveExposureParams {
    /// Max projects to return (default 100, max 1000).
    #[serde(default)]
    pub limit: Option<i64>,
}
