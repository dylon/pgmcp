//! Developer-tool ("toolbox") catalog parameter types.
//!
//! Re-exported by `params/mod.rs` and, transitively, by `server.rs` so
//! `crate::mcp::server::<Name>Params` resolves for the tool body files and the
//! `dispatch_tool!` / CLI paths.
#![allow(unused_imports)]

use super::*;
use rmcp::schemars;
use serde::Deserialize;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ToolboxSearchParams {
    #[schemars(
        description = "Task or capability query to match against the installed-tools catalog \
                       (e.g. 'prove this rewrite system terminates', 'find where threads block', \
                       'profile heap memory growth')."
    )]
    pub query: String,
    #[schemars(description = "Maximum number of tool cards to return (default: 10)")]
    pub limit: Option<i32>,
    #[schemars(description = "Filter by domain: formal_verification or developer_tooling")]
    pub domain: Option<String>,
    #[schemars(
        description = "Filter by category slug, e.g. proof_assistant, smt_solver, model_checker, \
                       termination_complexity, cpu_profiler, memory_profiler, ebpf_tracer, debugger"
    )]
    pub category: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ToolboxGetParams {
    #[schemars(description = "Tool card slug (e.g. 'z3', 'valgrind-massif') or numeric id")]
    pub slug_or_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ToolboxListParams {
    #[schemars(description = "Filter by domain: formal_verification or developer_tooling")]
    pub domain: Option<String>,
    #[schemars(description = "Filter by category slug")]
    pub category: Option<String>,
    #[schemars(description = "Maximum number of rows (default: 50)")]
    pub limit: Option<i32>,
    #[schemars(description = "Offset for pagination (default: 0)")]
    pub offset: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ToolboxRecommendParams {
    #[schemars(
        description = "Task to find tools for, e.g. 'prove a Rust function never panics' or \
                       'diagnose lock contention'. Ranked installed tool cards are returned."
    )]
    pub task: String,
    #[schemars(
        description = "Optional domain hint: formal_verification or developer_tooling. \
                       If omitted, inferred from the task text (and both domains otherwise)."
    )]
    pub domain: Option<String>,
    #[schemars(description = "Optional constraints / preferences to bias the ranking")]
    pub constraints: Option<Vec<String>>,
    #[schemars(description = "Maximum number of recommended tools (default: 8)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ToolboxRefreshParams {
    #[schemars(
        description = "Refresh mode: seed_only (re-upsert bundled cards; the cron re-embeds changed \
                       rows) or reembed (re-upsert AND synchronously embed any NULL-embedding cards \
                       for immediate availability). Default: seed_only."
    )]
    pub mode: Option<String>,
    #[schemars(description = "If true, report what would be seeded without changing the DB")]
    pub dry_run: Option<bool>,
}
