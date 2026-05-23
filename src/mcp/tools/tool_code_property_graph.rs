//! `tool_code_property_graph` (Phase 8).
use std::sync::Arc;
use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::code_analysis::cpg::build_cpg;
use crate::context::SystemContext;
use crate::mcp::server::CodePropertyGraphParams;
use crate::mcp::tools::sota_helpers::json_result;

pub async fn run(
    ctx: &SystemContext,
    params: CodePropertyGraphParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let cpg = match params.language.as_str() {
        "python" => build_cpg(
            &params.code,
            Arc::new(libgrammstein::code::languages::python::Python),
        ),
        other => {
            return Err(McpError::invalid_params(
                format!("code_property_graph: unsupported language `{other}` (currently: python)"),
                None,
            ));
        }
    }
    .map_err(|e| McpError::internal_error(format!("CPG build: {e}"), None))?;
    json_result(&json!({
        "language": params.language,
        "node_count": cpg.node_count(),
        "edge_count": cpg.edge_count(),
    }))
}
