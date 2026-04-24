//! `tool_complexity_hotspots` — MCP tool body, extracted from `super::super::server`.

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

pub async fn tool_complexity_hotspots(
    ctx: &SystemContext,
    params: ComplexityHotspotsParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().complexity_scans.fetch_add(1, Ordering::Relaxed);

    let limit = params.limit.unwrap_or(20);
    let sort_by = params.sort_by.as_deref().unwrap_or("composite");

    info!(
        tool = "complexity_hotspots",
        project = %params.project,
        limit,
        sort_by,
        "MCP tool invoked",
    );

    let file_data = ctx
        .db()
        .get_file_complexity_data(&params.project)
        .await
        .map_err(|e| McpError::internal_error(format!("Complexity query failed: {}", e), None))?;

    if file_data.is_empty() {
        return Ok(CallToolResult::success(vec![Content::text(
            "No indexed files found for this project.",
        )]));
    }

    // Get coupling data if git history is available
    let coupling_map: std::collections::HashMap<String, (f64, usize)> = {
        let coupling_pairs = ctx
            .db()
            .find_coupled_files(&params.project, 0.3, 3)
            .await
            .unwrap_or_default();

        let mut map: std::collections::HashMap<String, (f64, usize)> =
            std::collections::HashMap::new();
        for pair in &coupling_pairs {
            {
                let entry = map.entry(pair.file_a.clone()).or_insert((0.0, 0));
                if pair.jaccard > entry.0 {
                    entry.0 = pair.jaccard;
                }
                entry.1 += 1;
            }
            {
                let entry = map.entry(pair.file_b.clone()).or_insert((0.0, 0));
                if pair.jaccard > entry.0 {
                    entry.0 = pair.jaccard;
                }
                entry.1 += 1;
            }
        }
        map
    };

    // Find max values for normalization
    let max_chunks = file_data.iter().map(|f| f.chunk_count).max().unwrap_or(1) as f64;
    let max_topics = file_data.iter().map(|f| f.topic_count).max().unwrap_or(1) as f64;
    let max_size = file_data.iter().map(|f| f.size_bytes).max().unwrap_or(1) as f64;
    let max_coupling = coupling_map
        .values()
        .map(|(c, _)| *c)
        .fold(0.0f64, f64::max)
        .max(0.001);

    // Score each file
    let mut scored: Vec<serde_json::Value> = file_data
        .iter()
        .map(|f| {
            let (file_max_coupling, coupled_file_count) =
                coupling_map.get(&f.path).copied().unwrap_or((0.0, 0));

            let norm_chunks = f.chunk_count as f64 / max_chunks;
            let norm_topics = f.topic_count as f64 / max_topics;
            let norm_size = f.size_bytes as f64 / max_size;
            let norm_coupling = file_max_coupling / max_coupling;

            let composite =
                0.30 * norm_chunks + 0.25 * norm_topics + 0.25 * norm_size + 0.20 * norm_coupling;

            serde_json::json!({
                "path": f.path,
                "language": f.language,
                "size_bytes": f.size_bytes,
                "chunk_count": f.chunk_count,
                "topic_count": f.topic_count,
                "max_coupling": format!("{:.4}", file_max_coupling),
                "coupled_files": coupled_file_count,
                "composite_score": format!("{:.4}", composite),
            })
        })
        .collect();

    // Sort by the selected metric
    match sort_by {
        "size" => scored.sort_by(|a, b| {
            let sa = a["size_bytes"].as_i64().unwrap_or(0);
            let sb = b["size_bytes"].as_i64().unwrap_or(0);
            sb.cmp(&sa)
        }),
        "chunks" => scored.sort_by(|a, b| {
            let sa = a["chunk_count"].as_i64().unwrap_or(0);
            let sb = b["chunk_count"].as_i64().unwrap_or(0);
            sb.cmp(&sa)
        }),
        "topics" => scored.sort_by(|a, b| {
            let sa = a["topic_count"].as_i64().unwrap_or(0);
            let sb = b["topic_count"].as_i64().unwrap_or(0);
            sb.cmp(&sa)
        }),
        "coupling" => scored.sort_by(|a, b| {
            let sa: f64 = a["max_coupling"]
                .as_str()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0.0);
            let sb: f64 = b["max_coupling"]
                .as_str()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0.0);
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        }),
        _ => scored.sort_by(|a, b| {
            let sa: f64 = a["composite_score"]
                .as_str()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0.0);
            let sb: f64 = b["composite_score"]
                .as_str()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0.0);
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        }),
    }

    scored.truncate(limit as usize);

    let result = serde_json::json!({
        "project": params.project,
        "sort_by": sort_by,
        "file_count": scored.len(),
        "hotspots": scored,
        "guidance": "Files with high composite scores are prime candidates for refactoring. \
                     High topic diversity suggests the file handles too many concerns (SRP violation). \
                     High coupling with many files indicates the file is a change bottleneck.",
    });

    let json = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "complexity_hotspots",
        hotspots = scored.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
