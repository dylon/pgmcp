//! `tool_correct_query` (Phase 8) — wraps llammer-pipeline.
use std::sync::atomic::Ordering;

use llammer_pipeline::lattice::{LatticeCorrectionPipeline, LatticePipelineConfig};
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::server::CorrectQueryParams;
use crate::mcp::tools::sota_helpers::json_result;

pub async fn run(
    ctx: &SystemContext,
    params: CorrectQueryParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let cfg = LatticePipelineConfig {
        enable_phonetic: false,
        ..LatticePipelineConfig::default()
    };
    let mut pipeline = LatticeCorrectionPipeline::new(cfg)
        .map_err(|e| McpError::internal_error(format!("pipeline init: {e}"), None))?;
    let result = pipeline
        .correct(&params.query)
        .map_err(|e| McpError::internal_error(format!("correct: {e}"), None))?;
    json_result(&json!({
        "input": params.query,
        "corrected": result.text,
        "changed": result.changed,
        "confidence": result.confidence,
        "alternatives": result.alternatives.iter().map(|a| json!({
            "text": a.text, "weight": a.weight, "rank": a.rank
        })).collect::<Vec<_>>(),
    }))
}
