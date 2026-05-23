//! `tool_subtree_mining` (Phase 8).
use std::sync::Arc;
use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::code_analysis::subtree::mine_patterns;
use crate::context::SystemContext;
use crate::mcp::server::SubtreeMiningParams;
use crate::mcp::tools::sota_helpers::json_result;

pub async fn run(
    ctx: &SystemContext,
    params: SubtreeMiningParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let min_support = params.min_support.unwrap_or(0.1);
    let result = match params.language.as_str() {
        "python" => mine_patterns(
            Arc::new(libgrammstein::code::languages::python::Python),
            &params.sources,
            min_support,
        ),
        other => {
            return Err(McpError::invalid_params(
                format!("subtree_mining: unsupported language `{other}`"),
                None,
            ));
        }
    }
    .map_err(|e| McpError::internal_error(format!("subtree mine: {e}"), None))?;
    json_result(&json!({
        "language": params.language,
        "num_trees": result.num_trees,
        "patterns_found": result.patterns.len(),
        "candidates_generated": result.candidates_generated,
        "patterns_pruned": result.patterns_pruned,
        "mining_time_ms": result.mining_time_ms,
    }))
}
