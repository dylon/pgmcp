//! `tool_time_series_fuzzy_match` (Phase 8).
use std::sync::atomic::Ordering;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::fuzzy::time_series::{CommitCadenceSeries, TimeSeriesIndex};
use crate::mcp::server::TimeSeriesFuzzyMatchParams;
use crate::mcp::tools::sota_helpers::json_result;

pub async fn run(
    ctx: &SystemContext,
    params: TimeSeriesFuzzyMatchParams,
) -> Result<CallToolResult, McpError> {
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let mut idx = TimeSeriesIndex::default();
    for entry in &params.library {
        idx.push(CommitCadenceSeries {
            file_id: entry.id,
            series: entry.series.clone(),
        });
    }
    let k = params.k.unwrap_or(5) as usize;
    let near = idx.nearest(&params.probe, k);
    json_result(&json!({
        "k": k,
        "nearest": near.into_iter().map(|(id, d)| json!({"id": id, "distance": d}))
            .collect::<Vec<_>>(),
    }))
}
