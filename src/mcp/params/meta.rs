//! Parameter types for the adaptive-tool-surface meta-tools (`tool_catalog`,
//! `enable_tools`, `disable_tools`, `call_tool`).
//!
//! Re-exported by `params/mod.rs` and, transitively, by `server.rs` so
//! `crate::mcp::server::<Name>Params` resolves for the tool body files and the
//! `dispatch_tool!` / CLI paths.
#![allow(unused_imports)]

use super::*;
use rmcp::schemars;
use serde::Deserialize;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ToolCatalogParams {
    #[schemars(
        description = "Natural-language description of what you want to do (e.g. 'detect lock \
                       cycles', 'record progress on a work item', 'find who calls this function'). \
                       Ranks the full tool catalog semantically. Omit to browse by domain or list \
                       everything."
    )]
    pub query: Option<String>,
    #[schemars(
        description = "Restrict to one domain, e.g. core, graph_core, graph_func, work_items_a, \
                       work_items_b, concurrency, memory_search, security, ontology."
    )]
    pub domain: Option<String>,
    #[schemars(description = "Maximum number of tools to return (default: 12).")]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EnableToolsParams {
    #[schemars(
        description = "Exact tool names to enable for THIS session (they then appear natively in \
                       tools/list). Combine with `domain` and/or `query` — the union is enabled."
    )]
    #[serde(default)]
    pub names: Vec<String>,
    #[schemars(description = "Enable every tool in this domain (e.g. 'graph_func').")]
    pub domain: Option<String>,
    #[schemars(
        description = "Enable the top semantic matches for this natural-language query (use \
                       tool_catalog first if you want to preview them)."
    )]
    pub query: Option<String>,
    #[schemars(description = "Max tools to enable when using `query` (default: 5).")]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DisableToolsParams {
    #[schemars(
        description = "Tool names to remove from this session's enabled overlay (they revert to \
                       hidden unless they are in your learned defaults or mandatory core)."
    )]
    #[serde(default)]
    pub names: Vec<String>,
    #[schemars(
        description = "If true, clear ALL session-enabled tools, returning the surface to your \
                       learned defaults. Default: false."
    )]
    #[serde(default)]
    pub all: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CallToolParams {
    #[schemars(
        description = "Exact name of the tool to invoke (from tool_catalog). A direct fallback for \
                       reaching any tool without first enable_tools-ing it — useful if your client \
                       does not refresh tools/list on a list_changed notification."
    )]
    pub name: String,
    #[schemars(
        description = "Arguments object matching that tool's input schema (e.g. {\"project\": \
                       \"pgmcp\", \"limit\": 10}). Default: {}."
    )]
    #[serde(default)]
    pub args: serde_json::Value,
}
