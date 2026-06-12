//! Parameters for the `work_summary` MCP tool — a deterministic, multi-format
//! summary of a time period's work (typically a month) across the git repos in a
//! workspace. Extracted-style verbatim from the `params/` split; re-exported by
//! `params/mod.rs` so `crate::mcp::server::WorkSummaryParams` resolves for the
//! tool body and the CLI dispatch path.
#![allow(unused_imports)]

use super::*;
use rmcp::schemars;
use serde::Deserialize;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkSummaryParams {
    #[schemars(
        description = "Absolute path (or '~'-prefixed) to the workspace whose git repos to \
                       summarize, e.g. '/home/you/Workspace/project'. Defaults to the first \
                       configured [workspace] path."
    )]
    pub workspace_root: Option<String>,
    #[schemars(
        description = "Convenience period as 'YYYY-MM' — the whole calendar month (UTC). \
                       Ignored when both `since` and `until` are given."
    )]
    pub month: Option<String>,
    #[schemars(
        description = "Window start, inclusive — 'YYYY-MM-DD' or RFC3339. Use with `until`."
    )]
    pub since: Option<String>,
    #[schemars(description = "Window end, exclusive — 'YYYY-MM-DD' or RFC3339.")]
    pub until: Option<String>,
    #[schemars(
        description = "Author filter: a git `--author` regex (case-insensitive) selecting 'my \
                       work'. Default = the local git user.name. Pass 'all' for every contributor."
    )]
    pub author: Option<String>,
    #[schemars(description = "Output format: markdown|org|json (default markdown).")]
    pub format: Option<String>,
    #[schemars(
        description = "Primary grouping: 'project' (default), 'theme' (by conventional-commit \
                       type), or 'week' (ISO week). The per-project breakdown is always included."
    )]
    pub group_by: Option<String>,
    #[schemars(
        description = "Include the uncommitted / mid-stream working-tree section (default true)."
    )]
    pub include_uncommitted: Option<bool>,
    #[schemars(
        description = "Temporal-graph enrichment: 'auto' (default — used per project only when the \
                       index is fresh), 'on', or 'off'."
    )]
    pub use_graph: Option<String>,
    #[schemars(
        description = "Polish each project's bullets with the local LLM backend (default false; \
                       deterministic extractive bullets otherwise). Off keeps the artifact \
                       reproducible."
    )]
    pub narrative: Option<bool>,
    #[schemars(description = "Max repos to scan (default 200, clamped 1..=1000).")]
    pub max_repos: Option<u32>,
    #[schemars(
        description = "Max projects in the rendered output, busiest first (default 100, clamped 1..=1000)."
    )]
    pub limit: Option<u32>,
}
