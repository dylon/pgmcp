//! `tool_find_similar_modules` — MCP tool body, extracted from `super::super::server`.

#![allow(unused_imports)]

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, LoggingLevel};
use serde_json::json;
use tracing::{debug, error, info, warn};

use crate::context::SystemContext;
use crate::mcp::server::*;

pub async fn tool_find_similar_modules(
    ctx: &SystemContext,
    params: FindSimilarModulesParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let min_sim = params.min_similarity.unwrap_or(0.80);
    let limit = params.limit.unwrap_or(20);
    info!(
        tool = "find_similar_modules",
        project = %params.project,
        module_path = %params.module_path,
        min_similarity = min_sim,
        limit,
        "MCP tool invoked",
    );

    // Find files matching the module path pattern
    let source_files = ctx
        .db()
        .find_files_by_path_pattern(&params.project, &params.module_path)
        .await
        .map_err(|e| McpError::internal_error(format!("File lookup failed: {}", e), None))?;

    if source_files.is_empty() {
        return Ok(CallToolResult::success(vec![Content::text(format!(
            "No files matching '{}' found in project '{}'",
            params.module_path, params.project
        ))]));
    }

    let mut all_results = Vec::new();
    for src_file in &source_files {
        let similar = ctx
            .db()
            .find_similar_files(
                src_file.file_id,
                min_sim,
                limit,
                params.target_project.as_deref(),
                params.include_same_repo.unwrap_or(false),
            )
            .await
            .map_err(|e| {
                McpError::internal_error(format!("Similarity query failed: {}", e), None)
            })?;

        for sim in similar {
            all_results.push(serde_json::json!({
                "source_file": src_file.relative_path,
                "source_project": src_file.project_name,
                "similar_file": sim.path_b,
                "similar_project": sim.project_name_b,
                "language": sim.language,
                "avg_similarity": format!("{:.4}", sim.avg_similarity),
                "max_similarity": format!("{:.4}", sim.max_similarity),
                "matching_chunks": sim.matching_chunks,
            }));
        }
    }

    // Sort by avg_similarity descending and limit
    all_results.sort_by(|a, b| {
        let sa: f64 = a["avg_similarity"]
            .as_str()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0.0);
        let sb: f64 = b["avg_similarity"]
            .as_str()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0.0);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });
    all_results.truncate(limit as usize);

    let result = serde_json::json!({
        "source_files": source_files.iter().map(|f| &f.relative_path).collect::<Vec<_>>(),
        "source_project": params.project,
        "similar_modules": all_results,
        "result_count": all_results.len(),
    });

    let json = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "find_similar_modules",
        results = all_results.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
