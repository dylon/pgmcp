//! Parameter types for the `security_scan` tool — running installed external
//! security scanners over indexed projects and querying their findings.
//!
//! Re-exported by `params/mod.rs` and, transitively, by `server.rs` so
//! `crate::mcp::server::SecurityScanParams` resolves for the tool body and the
//! `dispatch_tool!` / CLI paths.
#![allow(unused_imports)]

use super::*;
use rmcp::schemars;
use serde::Deserialize;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SecurityScanParams {
    #[schemars(
        description = "Project name or path substring to scope to (default: all indexed projects)."
    )]
    pub project: Option<String>,
    #[schemars(
        description = "Subset of scanner slugs to report/run, e.g. [\"gitleaks\",\"trivy\",\"semgrep\"]. \
                       Default: every applicable installed scanner."
    )]
    pub scanners: Option<Vec<String>>,
    #[schemars(
        description = "When true, RUN the scanners now (subprocess sweep over the project) before \
                       returning results; otherwise return the cached findings (default false)."
    )]
    pub refresh: Option<bool>,
    #[schemars(
        description = "Minimum severity floor: low | medium | high | critical (default: no floor)."
    )]
    pub severity_min: Option<String>,
    #[schemars(
        description = "Include resolved (no-longer-seen) findings too (default false: only open)."
    )]
    pub include_resolved: Option<bool>,
    #[schemars(description = "Maximum findings to return (default 100, max 2000).")]
    pub limit: Option<i64>,
    #[schemars(
        description = "Finding class to query: \"security\" (default) | \"lint\". Lint findings \
                       are posted by the crucible linter loop (ADR-014) via POST /api/scanner/findings."
    )]
    pub finding_class: Option<String>,
}
