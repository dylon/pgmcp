//! `tool_articulatory_distance` (Phase 8).
use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::fuzzy::phonetic::articulatory_distance_score;
use crate::mcp::server::ArticulatoryDistanceParams;
use crate::mcp::tools::sota_helpers::json_result;

pub async fn run(
    ctx: &SystemContext,
    params: ArticulatoryDistanceParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    json_result(&json!({
        "a": params.a,
        "b": params.b,
        "articulatory_distance": articulatory_distance_score(&params.a, &params.b),
    }))
}
