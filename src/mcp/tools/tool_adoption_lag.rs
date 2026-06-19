//! `tool_adoption_lag` — find legacy usages of a modern reference file.
//!
//! Embeds each chunk of the reference file, runs HNSW kNN against the
//! corpus, filters by minimum similarity and minimum age (the legacy file
//! must be older than `legacy_min_age_days` since its last commit, per
//! the indexed_at timestamp).

#![allow(unused_imports)]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::time::Instant;

use chrono::Utc;
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::json;
use tracing::debug;

use crate::context::SystemContext;
use crate::db::queries;
use crate::mcp::server::*;
use crate::mcp::tools::fix_actions::{EstimatedEffort, FixAction, RecommendedFix, TargetPath};
use crate::mcp::tools::fix_helpers::pool_or_err;

const EF_SEARCH_DEFAULT: i32 = 200;

pub async fn tool_adoption_lag(
    ctx: &SystemContext,
    params: AdoptionLagParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .adoption_lag_scans
        .fetch_add(1, Ordering::Relaxed);

    let min_similarity = params.min_similarity.unwrap_or(0.70).clamp(0.0, 1.0);
    let legacy_min_age_days = params.legacy_min_age_days.unwrap_or(180).max(0);
    let limit = params.limit.unwrap_or(30).max(1);
    let _worktree_filter = params.worktree_filter.as_deref().unwrap_or("main");

    debug!(
        tool = "adoption_lag",
        new_file = %params.new_file,
        min_similarity,
        legacy_min_age_days,
        limit,
        "MCP tool invoked",
    );

    let pool = pool_or_err(ctx)?;
    let new_file = queries::resolve_file_reference(pool, &params.new_file)
        .await
        .map_err(|e| McpError::internal_error(format!("File-resolve failed: {}", e), None))?
        .ok_or_else(|| {
            McpError::invalid_params(
                format!("Reference file not found: {}", params.new_file),
                None,
            )
        })?;

    // Phase 5 C7: signature-aware column resolution.
    let active = crate::embed::signature::read_active_signature(pool)
        .await
        .map_err(|e| {
            McpError::internal_error(format!("active embedding signature: {}", e), None)
        })?;
    let col = active.read_column();

    // Pull all chunks of the reference file. We use those chunks as the kNN seeds.
    #[derive(sqlx::FromRow)]
    struct ChunkRow {
        embedding: pgvector::Vector,
    }
    let sql = format!(
        "SELECT {col} AS embedding FROM file_chunks
         WHERE file_id = $1 AND {col} IS NOT NULL
         ORDER BY chunk_index ASC"
    );
    let chunks: Vec<ChunkRow> = sqlx::query_as::<_, ChunkRow>(sqlx::AssertSqlSafe(sql.as_str()))
        .bind(new_file.file_id)
        .fetch_all(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("Chunk fetch failed: {}", e), None))?;

    if chunks.is_empty() {
        return Err(McpError::invalid_params(
            format!("Reference file has no indexed chunks: {}", params.new_file),
            None,
        ));
    }

    // For each seed chunk, run kNN; aggregate by (file_id, similarity).
    let mut by_file: HashMap<i64, (f64, queries::SearchResult)> = HashMap::new();
    for c in &chunks {
        let embedding: Vec<f32> = c.embedding.to_vec();
        let results = queries::semantic_search(
            pool,
            &embedding,
            limit,
            None,
            params.project.as_deref(),
            EF_SEARCH_DEFAULT,
            true,
        )
        .await
        .map_err(|e| McpError::internal_error(format!("kNN failed: {}", e), None))?;
        for r in results {
            // Skip the new_file itself.
            if r.relative_path == new_file.relative_path && r.project_name == new_file.project_name
            {
                continue;
            }
            let score = r.score.unwrap_or(0.0);
            if score < min_similarity {
                continue;
            }
            let entry = by_file
                .entry(r.start_line as i64 + r.end_line as i64) // weak key; replaced below
                .or_insert((score, r.clone()));
            if score > entry.0 {
                *entry = (score, r);
            }
        }
    }

    // Re-key by (project, path) — safer than the weak line-sum key above.
    let mut deduped: HashMap<(String, String), (f64, queries::SearchResult)> = HashMap::new();
    for (_, (score, r)) in by_file {
        let key = (r.project_name.clone(), r.relative_path.clone());
        deduped
            .entry(key)
            .and_modify(|e| {
                if score > e.0 {
                    *e = (score, r.clone());
                }
            })
            .or_insert((score, r));
    }

    // Filter by age. We approximate "last touch" via indexed_files.modified_at.
    let mut output: Vec<serde_json::Value> = Vec::new();
    for ((_proj, _path), (sim, r)) in &deduped {
        let age_row: Option<chrono::DateTime<Utc>> = sqlx::query_scalar(
            "SELECT modified_at FROM indexed_files f
             JOIN projects p ON p.id = f.project_id
             WHERE p.name = $1 AND f.relative_path = $2",
        )
        .bind(&r.project_name)
        .bind(&r.relative_path)
        .fetch_optional(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("Age lookup failed: {}", e), None))?;
        let age_days = age_row
            .map(|t| (Utc::now() - t).num_days().max(0))
            .unwrap_or(0);
        if age_days < legacy_min_age_days as i64 {
            continue;
        }
        let action = if *sim >= 0.92 {
            FixAction::MergeFiles
        } else if *sim >= 0.75 {
            FixAction::MoveFunction
        } else {
            FixAction::AddTest
        };
        let recommendation = match action {
            FixAction::MergeFiles => format!(
                "Strong similarity ({:.2}) — merge {} into {}.",
                sim, r.relative_path, params.new_file
            ),
            FixAction::MoveFunction => format!(
                "Moderate similarity ({:.2}) — replace {}'s local copy with imports from {}.",
                sim, r.relative_path, params.new_file
            ),
            _ => format!(
                "Low-moderate similarity ({:.2}) — flag for human review of {}.",
                sim, r.relative_path
            ),
        };
        let fix = RecommendedFix::new(action, r.project_name.clone())
            .with_confidence(0.50 + (*sim * 0.3).min(0.3))
            .with_effort(EstimatedEffort::Medium)
            .add_step(recommendation.clone());
        let fix_json = serde_json::to_value(&fix).map_err(|e| {
            McpError::internal_error(format!("Fix serialization failed: {}", e), None)
        })?;
        output.push(json!({
            "path": r.relative_path,
            "project": r.project_name,
            "similarity_to_new": format!("{:.4}", sim),
            "age_days": age_days,
            "why_legacy": format!(
                "Indexed {} days ago; similar to modern reference at sim {:.2}.",
                age_days, sim
            ),
            "recommendation": recommendation,
            "recommended_fix": fix_json,
        }));
        if output.len() >= limit as usize {
            break;
        }
    }

    output.sort_by(|a, b| {
        let sa: f64 = a["similarity_to_new"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        let sb: f64 = b["similarity_to_new"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });

    // Shadow-ASR channel (Phase D2b): per-effect symbol-count breakdown
    // for the project. Universal enrichment — every tool benefits from
    // surfacing the effect distribution alongside its primary output.
    // Gracefully degrades to empty when the project lookup or
    // shadow-ASR data isn't populated.
    let effect_breakdown: Vec<serde_json::Value> = (async {
        let Some(pool) = ctx.db().pool() else {
            return Vec::new();
        };
        let project_id_opt: Option<i32> =
            sqlx::query_scalar("SELECT id FROM projects WHERE name = $1")
                .bind(&params.project)
                .fetch_optional(pool)
                .await
                .unwrap_or(None);
        match project_id_opt {
            Some(pid) => crate::mcp::tools::sema_helpers::effects::effect_counts(pool, pid)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
                .collect(),
            None => Vec::new(),
        }
    })
    .await;

    let result = json!({
        "effect_breakdown": effect_breakdown,
        "new_file": params.new_file,
        "legacy_usages": output,
        "total_legacy_usages": output.len(),
        "parameters": {
            "min_similarity": min_similarity,
            "legacy_min_age_days": legacy_min_age_days,
            "limit": limit,
        },
        "guidance": "Each row carries a typed `recommended_fix`. similarity ≥ 0.92 → merge_files; \
                     0.75-0.92 → move_function; 0.70-0.75 → add_test (flag for review).",
    });
    let json_str = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "adoption_lag",
        candidates = output.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json_str)]))
}
