//! `tool_refactoring_report` — MCP tool body, extracted from `super::super::server`.

#![allow(unused_imports)]

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, LoggingLevel};
use serde_json::json;
use tracing::debug;

use crate::context::SystemContext;
use crate::mcp::server::*;

const DEFAULT_REFACTORING_REPORT_MIN_SIMILARITY: f64 = 0.85;
const DEFAULT_REFACTORING_REPORT_MIN_PROJECTS: usize = 2;
const DEFAULT_REFACTORING_REPORT_LIMIT: i32 = 20;
const MAX_REFACTORING_REPORT_LIMIT: i32 = 100;
const MAX_REFACTORING_REPORT_MIN_PROJECTS: usize = 128;
const MAX_REFACTORING_REPORT_LANGUAGE_BYTES: usize = 64;
const REFACTORING_REPORT_FETCH_MULTIPLIER: i32 = 5;

pub async fn tool_refactoring_report(
    ctx: &SystemContext,
    params: RefactoringReportParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let min_sim = normalize_min_similarity(params.min_similarity)?;
    let min_projects = params
        .min_projects
        .unwrap_or(DEFAULT_REFACTORING_REPORT_MIN_PROJECTS)
        .clamp(1, MAX_REFACTORING_REPORT_MIN_PROJECTS);
    let language = normalize_language_filter(params.language)?;
    let limit = params
        .limit
        .unwrap_or(DEFAULT_REFACTORING_REPORT_LIMIT)
        .clamp(1, MAX_REFACTORING_REPORT_LIMIT) as usize;
    let fetch_limit = (limit as i32).saturating_mul(REFACTORING_REPORT_FETCH_MULTIPLIER);
    let include_same_repo = params.include_same_repo.unwrap_or(false);
    debug!(
        tool = "refactoring_report",
        min_similarity = min_sim,
        min_projects,
        language = language.as_deref().unwrap_or("*"),
        limit,
        fetch_limit,
        include_same_repo,
        "MCP tool invoked",
    );

    let pairs = ctx
        .db()
        .find_duplicate_file_pairs(min_sim, language.as_deref(), fetch_limit, include_same_repo)
        .await
        .map_err(|e| McpError::internal_error(format!("Duplicate query failed: {}", e), None))?;

    let clusters = cluster_file_pairs(&pairs, min_projects);

    // Enrich clusters with refactoring metadata
    let mut candidates: Vec<serde_json::Value> = Vec::new();
    for cluster in clusters.iter().take(limit) {
        let empty_arr = Vec::new();
        let files = cluster["files"].as_array().unwrap_or(&empty_arr).clone();
        let projects_arr = cluster["projects"].as_array().cloned().unwrap_or_default();
        let projects: Vec<&str> = projects_arr.iter().filter_map(|v| v.as_str()).collect();
        let avg_sim: f64 = cluster["avg_similarity"]
            .as_str()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0.0);

        // Infer crate name from common path segments
        let paths: Vec<&str> = files
            .iter()
            .filter_map(|f| f["relative_path"].as_str())
            .collect();
        let suggested_name = infer_crate_name(&paths);

        // Estimate shared lines (smallest file in cluster)
        let min_lines: i64 = files
            .iter()
            .filter_map(|f| f["line_count"].as_i64())
            .min()
            .unwrap_or(0);

        let score = projects.len() as f64 * avg_sim;

        candidates.push(serde_json::json!({
            "suggested_crate_name": suggested_name,
            "language": cluster["language"],
            "projects": projects,
            "project_count": projects.len(),
            "avg_similarity": cluster["avg_similarity"],
            "estimated_shared_lines": min_lines,
            "score": format!("{:.2}", score),
            "files": files,
        }));
    }

    // Sort by score descending
    candidates.sort_by(|a, b| {
        let sa: f64 = a["score"].as_str().unwrap_or("0").parse().unwrap_or(0.0);
        let sb: f64 = b["score"].as_str().unwrap_or("0").parse().unwrap_or(0.0);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });

    // Shadow-ASR channel (Phase D2b): workspace-wide effect distribution
    // (sum across all projects). Gives consumers a baseline against which
    // their tool-specific output's effect concentration can be compared.
    let effect_breakdown: Vec<serde_json::Value> = (async {
        let Some(pool) = ctx.db().pool() else {
            return Vec::new();
        };
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT se.effect, COUNT(*)::int8
             FROM symbol_effects se
             GROUP BY se.effect
             ORDER BY se.effect",
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        rows.into_iter()
            .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
            .collect()
    })
    .await;

    let result = serde_json::json!({
        "effect_breakdown": effect_breakdown,
        "candidates": candidates,
        "total_candidates": candidates.len(),
        "parameters": {
            "min_similarity": min_sim,
            "min_projects": min_projects,
            "language": language,
            "limit": limit,
            "fetch_limit": fetch_limit,
            "include_same_repo": include_same_repo,
        },
    });

    let json = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "refactoring_report",
        candidates = candidates.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}

fn normalize_min_similarity(raw: Option<f64>) -> Result<f64, McpError> {
    let value = raw.unwrap_or(DEFAULT_REFACTORING_REPORT_MIN_SIMILARITY);
    if !value.is_finite() {
        return Err(McpError::invalid_params(
            "min_similarity must be finite",
            None,
        ));
    }
    Ok(value.clamp(0.0, 1.0))
}

fn normalize_language_filter(raw: Option<String>) -> Result<Option<String>, McpError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let language = raw.trim();
    if language.is_empty() {
        return Ok(None);
    }
    if language.len() > MAX_REFACTORING_REPORT_LANGUAGE_BYTES {
        return Err(McpError::invalid_params(
            format!("language must be at most {MAX_REFACTORING_REPORT_LANGUAGE_BYTES} bytes"),
            None,
        ));
    }
    Ok(Some(language.to_ascii_lowercase()))
}
