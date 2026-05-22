//! `tool_find_misplaced_code` — MCP tool body, extracted from `super::super::server`.

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

pub async fn tool_find_misplaced_code(
    ctx: &SystemContext,
    params: FindMisplacedCodeParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().misplaced_scans.fetch_add(1, Ordering::Relaxed);

    let min_mismatch = params.min_mismatch.unwrap_or(0.5);

    debug!(
        tool = "find_misplaced_code",
        project = %params.project,
        min_mismatch,
        "MCP tool invoked",
    );

    let rows = ctx
        .db()
        .load_chunk_topic_assignments_for_files(Some(&params.project))
        .await
        .map_err(|e| McpError::internal_error(format!("Query failed: {}", e), None))?;

    if rows.is_empty() {
        return Ok(CallToolResult::success(vec![Content::text(
            "No topic assignments found. Run discover_topics first.",
        )]));
    }

    // Build file → dominant topic map
    let mut file_dominant: std::collections::HashMap<String, (i32, String)> =
        std::collections::HashMap::new();
    for row in &rows {
        file_dominant
            .entry(row.path.clone())
            .or_insert((row.topic_id, row.topic_label.clone()));
    }

    // Build directory → topic distribution map
    let mut dir_topics: std::collections::HashMap<String, std::collections::HashMap<i32, usize>> =
        std::collections::HashMap::new();
    for (path, (topic_id, _)) in &file_dominant {
        let dir = path
            .rsplit_once('/')
            .map(|(d, _)| d.to_string())
            .unwrap_or_default();
        *dir_topics
            .entry(dir)
            .or_default()
            .entry(*topic_id)
            .or_insert(0) += 1;
    }

    // Score each file
    let mut misplaced: Vec<serde_json::Value> = Vec::new();
    for (path, (file_topic_id, file_topic_label)) in &file_dominant {
        let dir = path
            .rsplit_once('/')
            .map(|(d, _)| d.to_string())
            .unwrap_or_default();
        if let Some(topic_counts) = dir_topics.get(&dir) {
            let total_files: usize = topic_counts.values().sum();
            if total_files <= 1 {
                continue; // Can't determine mismatch with only one file
            }
            let file_topic_count = topic_counts.get(file_topic_id).copied().unwrap_or(0);
            let mismatch_score = 1.0 - (file_topic_count as f64 / total_files as f64);

            if mismatch_score >= min_mismatch {
                // Find the directory's majority topic
                let (majority_topic_id, _) = topic_counts
                    .iter()
                    .max_by_key(|(_, count)| *count)
                    .map(|(id, count)| (*id, *count))
                    .unwrap_or((0, 0));

                let majority_label = rows
                    .iter()
                    .find(|r| r.topic_id == majority_topic_id)
                    .map(|r| r.topic_label.as_str())
                    .unwrap_or("unknown");

                misplaced.push(serde_json::json!({
                    "path": path,
                    "directory": dir,
                    "file_topic": file_topic_label,
                    "directory_majority_topic": majority_label,
                    "mismatch_score": format!("{:.2}", mismatch_score),
                    "files_in_directory": total_files,
                }));
            }
        }
    }

    // Sort by mismatch score descending
    misplaced.sort_by(|a, b| {
        let sa: f64 = a["mismatch_score"]
            .as_str()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0.0);
        let sb: f64 = b["mismatch_score"]
            .as_str()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0.0);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });

    let result = serde_json::json!({
        "project": params.project,
        "min_mismatch": min_mismatch,
        "misplaced_count": misplaced.len(),
        "misplaced_files": misplaced,
        "guidance": "Files whose semantic content doesn't match their directory context. \
                     Consider moving misplaced files to directories matching their semantic \
                     content, or investigate if they serve a cross-cutting concern.",
    });

    let json = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "find_misplaced_code",
        misplaced = misplaced.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
