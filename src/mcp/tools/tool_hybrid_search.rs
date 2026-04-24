//! `tool_hybrid_search` — MCP tool body, extracted from `super::super::server`.

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

pub async fn tool_hybrid_search(
    ctx: &SystemContext,
    params: HybridSearchParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().hybrid_searches.fetch_add(1, Ordering::Relaxed);

    let limit = params.limit.unwrap_or(20);
    let bm25_weight = params.bm25_weight.unwrap_or(0.5);
    let semantic_weight = params.semantic_weight.unwrap_or(0.5);

    info!(
        tool = "hybrid_search",
        query = %truncate(&params.query, 200),
        project = params.project.as_deref().unwrap_or("*"),
        language = params.language.as_deref().unwrap_or("*"),
        limit,
        bm25_weight,
        semantic_weight,
        "MCP tool invoked",
    );

    // Run text search
    let text_results = ctx
        .db()
        .text_search(
            &params.query,
            limit * 2, // fetch more for fusion
            params.language.as_deref(),
        )
        .await
        .map_err(|e| McpError::internal_error(format!("Text search failed: {}", e), None))?;

    // Run semantic search
    let embedding = ctx
        .embed()
        .embed_query(&params.query)
        .await
        .map_err(|e| McpError::internal_error(format!("Embedding failed: {}", e), None))?;

    let ef_search = ctx.config().load().vector.ef_search;
    let semantic_results = ctx
        .db()
        .semantic_search(
            &embedding,
            limit * 2,
            params.language.as_deref(),
            params.project.as_deref(),
            ef_search,
        )
        .await
        .map_err(|e| McpError::internal_error(format!("Semantic search failed: {}", e), None))?;

    // Reciprocal Rank Fusion (RRF) with k=60
    let k = 60.0;
    let mut rrf_scores: std::collections::HashMap<String, (f64, serde_json::Value)> =
        std::collections::HashMap::new();

    // Score text search results
    for (rank, result) in text_results.iter().enumerate() {
        let key = format!("text:{}:{}", result.relative_path, rank);
        let rrf = bm25_weight / (k + rank as f64 + 1.0);
        let snippet = result.content.as_deref().unwrap_or("");
        let entry = rrf_scores.entry(key).or_insert((
            0.0,
            serde_json::json!({
                "path": result.path,
                "relative_path": result.relative_path,
                "snippet": truncate(snippet, 300),
                "language": result.language,
                "source": "text",
            }),
        ));
        entry.0 += rrf;
    }

    // Score semantic search results
    for (rank, result) in semantic_results.iter().enumerate() {
        let key = format!("semantic:{}:{}", result.relative_path, result.start_line);
        let rrf = semantic_weight / (k + rank as f64 + 1.0);
        let entry = rrf_scores.entry(key).or_insert((
            0.0,
            serde_json::json!({
                "path": result.path,
                "relative_path": result.relative_path,
                "project_name": result.project_name,
                "start_line": result.start_line,
                "end_line": result.end_line,
                "snippet": truncate(&result.chunk_content, 300),
                "language": result.language,
                "source": "semantic",
            }),
        ));
        entry.0 += rrf;
    }

    // Sort by RRF score and take top results
    let mut fused: Vec<serde_json::Value> = rrf_scores
        .into_iter()
        .map(|(_, (score, mut val))| {
            if let Some(o) = val.as_object_mut() {
                o.insert(
                    "rrf_score".to_string(),
                    serde_json::json!(format!("{:.6}", score)),
                );
            }
            val
        })
        .collect();

    fused.sort_by(|a, b| {
        let sa: f64 = a["rrf_score"]
            .as_str()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0.0);
        let sb: f64 = b["rrf_score"]
            .as_str()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0.0);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });
    fused.truncate(limit as usize);

    let result = serde_json::json!({
        "query": params.query,
        "project": params.project,
        "language": params.language,
        "bm25_weight": bm25_weight,
        "semantic_weight": semantic_weight,
        "text_results": text_results.len(),
        "semantic_results": semantic_results.len(),
        "fused_count": fused.len(),
        "results": fused,
        "guidance": "RRF combines keyword precision with semantic recall. \
                     Increase bm25_weight for exact-match queries (error messages, function names). \
                     Increase semantic_weight for conceptual queries (design patterns, workflows).",
    });

    let json = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "hybrid_search",
        results = fused.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
