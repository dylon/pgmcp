//! `tool_suggest_splits` — MCP tool body, extracted from `super::super::server`.

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

pub async fn tool_suggest_splits(
    ctx: &SystemContext,
    params: SuggestSplitsParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().split_scans.fetch_add(1, Ordering::Relaxed);

    let language_param = params.language.as_deref().unwrap_or("markdown");
    let language_filter = if language_param == "*" {
        None
    } else {
        Some(language_param)
    };
    let min_entropy = params.min_entropy.unwrap_or(1.5);
    let min_topics = params.min_topics.unwrap_or(3) as usize;
    let limit = params.limit.unwrap_or(20);

    info!(
        tool = "suggest_splits",
        project = %params.project,
        language = language_param,
        min_entropy,
        min_topics,
        limit,
        "MCP tool invoked",
    );

    let rows = ctx
        .db()
        .get_chunk_topic_details(&params.project, language_filter)
        .await
        .map_err(|e| McpError::internal_error(format!("Query failed: {}", e), None))?;

    if rows.is_empty() {
        return Ok(CallToolResult::success(vec![Content::text(
            "No topic assignments found for the specified project/language. \
             Run discover_topics first.",
        )]));
    }

    // Group by file
    use std::collections::HashMap;

    struct FileChunkInfo {
        path: String,
        relative_path: String,
        language: String,
        line_count: i32,
        size_bytes: i64,
        chunks: Vec<ChunkEntry>,
    }

    struct ChunkEntry {
        chunk_index: i32,
        start_line: i32,
        content: String,
        // topic assignments for this chunk, sorted by membership_score descending
        topics: Vec<(i32, String, f64)>, // (topic_id, label, membership_score)
    }

    let mut file_map: HashMap<i64, FileChunkInfo> = HashMap::new();

    for row in &rows {
        let entry = file_map
            .entry(row.file_id)
            .or_insert_with(|| FileChunkInfo {
                path: row.path.clone(),
                relative_path: row.relative_path.clone(),
                language: row.language.clone(),
                line_count: row.line_count,
                size_bytes: row.size_bytes,
                chunks: Vec::new(),
            });

        // Find or create chunk entry
        if let Some(chunk) = entry
            .chunks
            .iter_mut()
            .find(|c| c.chunk_index == row.chunk_index)
        {
            chunk
                .topics
                .push((row.topic_id, row.topic_label.clone(), row.membership_score));
        } else {
            entry.chunks.push(ChunkEntry {
                chunk_index: row.chunk_index,
                start_line: row.start_line,
                content: row.chunk_content.clone(),
                topics: vec![(row.topic_id, row.topic_label.clone(), row.membership_score)],
            });
        }
    }

    // Sort chunks within each file by chunk_index
    for info in file_map.values_mut() {
        info.chunks.sort_by_key(|c| c.chunk_index);
    }

    // Compute entropy and filter candidates
    let heading_re = regex::Regex::new(r"^(#{1,6})\s+(.+)$").expect("valid heading regex");

    let mut candidates: Vec<serde_json::Value> = Vec::new();

    for info in file_map.values() {
        // Aggregate topic distribution across all chunks in this file
        let mut topic_membership: HashMap<i32, (f64, String)> = HashMap::new();
        for chunk in &info.chunks {
            for &(tid, ref label, score) in &chunk.topics {
                let entry = topic_membership.entry(tid).or_insert((0.0, label.clone()));
                entry.0 += score;
            }
        }

        let distinct_topics = topic_membership.len();
        if distinct_topics < min_topics {
            continue;
        }

        // Shannon entropy
        let total_membership: f64 = topic_membership.values().map(|(m, _)| m).sum();
        if total_membership <= 0.0 {
            continue;
        }

        let mut entropy = 0.0f64;
        let mut topic_dist: Vec<serde_json::Value> = Vec::new();

        for (tid, (membership, label)) in &topic_membership {
            let p = membership / total_membership;
            if p > 0.0 {
                entropy -= p * p.log2();
            }
            topic_dist.push(serde_json::json!({
                "topic_id": tid,
                "topic": label,
                "membership": format!("{:.2}", membership),
                "proportion": format!("{:.2}", p),
            }));
        }

        if entropy < min_entropy {
            continue;
        }

        // Sort topic distribution by proportion descending
        topic_dist.sort_by(|a, b| {
            let pa: f64 = a["proportion"]
                .as_str()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0.0);
            let pb: f64 = b["proportion"]
                .as_str()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0.0);
            pb.partial_cmp(&pa).unwrap_or(std::cmp::Ordering::Equal)
        });

        // Detect topic transitions (dominant topic changes between consecutive chunks)
        let mut suggested_splits: Vec<serde_json::Value> = Vec::new();

        let mut prev_dominant: Option<(i32, String)> = None;
        for chunk in &info.chunks {
            if let Some(&(tid, ref label, _)) = chunk.topics.first() {
                if let Some((prev_tid, ref prev_label)) = prev_dominant
                    && tid != prev_tid
                {
                    // Topic transition — look for nearest heading
                    let transition_line = chunk.start_line;

                    // Search backward through this chunk and the previous for headings
                    let mut nearest_heading: Option<(i32, String)> = None;
                    for line in chunk.content.lines() {
                        if let Some(caps) = heading_re.captures(line) {
                            let heading_text = caps
                                .get(2)
                                .map(|m| m.as_str().to_string())
                                .unwrap_or_default();
                            nearest_heading = Some((chunk.start_line, heading_text));
                            break;
                        }
                    }

                    // Generate suggested filename from heading
                    let suggested_filename = nearest_heading.as_ref().map(|(_, text)| {
                        let slug: String = text
                            .to_lowercase()
                            .chars()
                            .map(|c| if c.is_alphanumeric() { c } else { '-' })
                            .collect();
                        let slug = slug.trim_matches('-').to_string();
                        // Collapse consecutive dashes
                        let mut result = String::with_capacity(slug.len());
                        let mut prev_dash = false;
                        for c in slug.chars() {
                            if c == '-' {
                                if !prev_dash {
                                    result.push(c);
                                }
                                prev_dash = true;
                            } else {
                                result.push(c);
                                prev_dash = false;
                            }
                        }
                        format!("{}.md", result)
                    });

                    suggested_splits.push(serde_json::json!({
                        "transition_line": transition_line,
                        "topic_before": prev_label,
                        "topic_after": label,
                        "nearest_heading": nearest_heading.as_ref().map(|(_, h)| h.as_str()),
                        "heading_line": nearest_heading.as_ref().map(|(l, _)| l),
                        "suggested_filename": suggested_filename,
                    }));
                }
                prev_dominant = Some((tid, label.clone()));
            }
        }

        candidates.push(serde_json::json!({
            "path": info.path,
            "relative_path": info.relative_path,
            "language": info.language,
            "line_count": info.line_count,
            "size_bytes": info.size_bytes,
            "topic_count": distinct_topics,
            "entropy": format!("{:.2}", entropy),
            "topic_distribution": topic_dist,
            "topic_transitions": suggested_splits.len(),
            "suggested_splits": suggested_splits,
        }));
    }

    // Sort by entropy descending
    candidates.sort_by(|a, b| {
        let ea: f64 = a["entropy"].as_str().unwrap_or("0").parse().unwrap_or(0.0);
        let eb: f64 = b["entropy"].as_str().unwrap_or("0").parse().unwrap_or(0.0);
        eb.partial_cmp(&ea).unwrap_or(std::cmp::Ordering::Equal)
    });
    candidates.truncate(limit as usize);

    let result = serde_json::json!({
        "project": params.project,
        "language": language_param,
        "min_entropy": min_entropy,
        "min_topics": min_topics,
        "split_candidates_found": candidates.len(),
        "candidates": candidates,
        "guidance": "Files with high entropy span many distinct topics. Split at heading \
                     boundaries that align with topic transitions for clean decomposition. \
                     Files with entropy > 2.0 are strong split candidates.",
    });

    let json = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "suggest_splits",
        candidates = candidates.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
