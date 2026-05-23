//! `tool_pattern_search` — embed a snippet, find cross-project matches, emit a verdict.
//!
//! Distinct from `semantic_search`: targets *snippet-as-query* rather than
//! natural-language queries, and produces a verdict (reuse / adapt / new) plus
//! a recommended action shape.

#![allow(unused_imports)]

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::time::Instant;

use chrono::Utc;
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::json;
use tracing::{debug, info};

use crate::context::SystemContext;
use crate::db::queries;
use crate::mcp::server::*;
use crate::mcp::tools::fix_actions::{EstimatedEffort, FixAction, RecommendedFix, TargetPath};
use crate::mcp::tools::fix_helpers::pool_or_err;

const EF_SEARCH_DEFAULT: i32 = 100;

pub async fn tool_pattern_search(
    ctx: &SystemContext,
    params: PatternSearchParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().pattern_searches.fetch_add(1, Ordering::Relaxed);

    let snippet = params.snippet.trim();
    if snippet.is_empty() {
        return Err(McpError::invalid_params(
            "pattern_search requires a non-empty snippet".to_string(),
            None,
        ));
    }
    let min_similarity = params.min_similarity.unwrap_or(0.78).clamp(0.0, 1.0);
    let limit = params.limit.unwrap_or(15).max(1);
    let _worktree_filter = params.worktree_filter.as_deref().unwrap_or("main");

    debug!(
        tool = "pattern_search",
        snippet_chars = snippet.len(),
        language = params.language.as_deref().unwrap_or("*"),
        min_similarity,
        limit,
        "MCP tool invoked",
    );

    let embed_start = Instant::now();
    let embedding =
        ctx.embed().embed_query(snippet).await.map_err(|e| {
            McpError::internal_error(format!("Snippet embedding failed: {}", e), None)
        })?;
    let embed_ms = embed_start.elapsed().as_millis() as u64;

    let pool = pool_or_err(ctx)?;
    let results = queries::semantic_search(
        pool,
        &embedding,
        limit * 2,
        params.language.as_deref(),
        None,
        EF_SEARCH_DEFAULT,
        true,
    )
    .await
    .map_err(|e| {
        McpError::internal_error(format!("Pattern-search HNSW kNN failed: {}", e), None)
    })?;

    // Filter by min_similarity and exclude_project, dedupe per (project, relative_path).
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut matches: Vec<serde_json::Value> = Vec::new();
    let mut distinct_projects: HashSet<String> = HashSet::new();
    let mut max_similarity = 0.0_f64;
    for r in results {
        let score = r.score.unwrap_or(0.0);
        if score < min_similarity {
            continue;
        }
        if let Some(excl) = params.exclude_project.as_deref()
            && r.project_name == excl
        {
            continue;
        }
        let key = (r.project_name.clone(), r.relative_path.clone());
        if !seen.insert(key) {
            continue;
        }
        distinct_projects.insert(r.project_name.clone());
        if score > max_similarity {
            max_similarity = score;
        }
        matches.push(json!({
            "project": r.project_name,
            "path": r.relative_path,
            "start_line": r.start_line,
            "end_line": r.end_line,
            "similarity": format!("{:.4}", score),
            "chunk_excerpt": truncate(&r.chunk_content, 240),
        }));
        if matches.len() >= limit as usize {
            break;
        }
    }

    let strong_hits = matches
        .iter()
        .filter(|m| {
            m["similarity"]
                .as_str()
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0)
                > 0.9
        })
        .count();
    let verdict = if strong_hits >= 3 && distinct_projects.len() >= 3 {
        "reuse"
    } else if matches.iter().any(|m| {
        m["similarity"]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0)
            > 0.85
    }) {
        "adapt"
    } else {
        "new"
    };

    let recommendation = match verdict {
        "reuse" => {
            "Strong cross-project match — extract a shared crate or import from one of the \
             existing projects. Run `extraction_candidates` for an effort + risk breakdown."
        }
        "adapt" => "Single strong match — import or copy from that file and adapt.",
        _ => "No high-similarity matches — proceed with new implementation.",
    };

    let recommended_fix = match verdict {
        "reuse" => {
            let fix = RecommendedFix::new(FixAction::ExtractModule, "shared")
                .with_confidence(0.55)
                .with_effort(EstimatedEffort::Medium)
                .add_step(format!(
                    "Run `extraction_candidates` with the projects from these {} matches to \
                     produce a concrete shared-crate extraction plan.",
                    matches.len()
                ));
            Some(serde_json::to_value(&fix).map_err(|e| {
                McpError::internal_error(format!("Fix serialization failed: {}", e), None)
            })?)
        }
        _ => None,
    };

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

    let result = json!({
        "effect_breakdown": effect_breakdown,
        "snippet_chars": snippet.len(),
        "embed_ms": embed_ms,
        "matches": matches,
        "cluster_count": matches.len(),
        "distinct_projects": distinct_projects.len(),
        "verdict": verdict,
        "recommendation": recommendation,
        "recommended_fix": recommended_fix,
        "health": json!({"embed_ok": true}),
    });
    let json_str = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "pattern_search",
        matches = matches.len(),
        verdict,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json_str)]))
}

fn truncate(s: &str, max_chars: usize) -> String {
    let mut end = s.len();
    for (count, (idx, _)) in s.char_indices().enumerate() {
        if count >= max_chars {
            end = idx;
            break;
        }
    }
    let body = &s[..end];
    if end < s.len() {
        format!("{}…", body)
    } else {
        body.to_string()
    }
}
