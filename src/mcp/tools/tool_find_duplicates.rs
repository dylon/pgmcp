//! `tool_find_duplicates` — MCP tool body, extracted from `super::super::server`.

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
use crate::mcp::tools::sema_helpers::equivalence::materialized_available;

const DEFAULT_FIND_DUPLICATES_MIN_SIMILARITY: f64 = 0.90;
const DEFAULT_FIND_DUPLICATES_MIN_PROJECTS: usize = 2;
const DEFAULT_FIND_DUPLICATES_LIMIT: i32 = 20;
const MAX_FIND_DUPLICATES_LIMIT: i32 = 100;
const MAX_FIND_DUPLICATES_MIN_PROJECTS: usize = 128;
const MAX_FIND_DUPLICATES_LANGUAGE_BYTES: usize = 64;
const FIND_DUPLICATES_FETCH_MULTIPLIER: i32 = 5;

pub async fn tool_find_duplicates(
    ctx: &SystemContext,
    params: FindDuplicatesParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let min_sim = normalize_min_similarity(params.min_similarity)?;
    let min_projects = params
        .min_projects
        .unwrap_or(DEFAULT_FIND_DUPLICATES_MIN_PROJECTS)
        .clamp(1, MAX_FIND_DUPLICATES_MIN_PROJECTS);
    let language = normalize_language_filter(params.language)?;
    let limit = params
        .limit
        .unwrap_or(DEFAULT_FIND_DUPLICATES_LIMIT)
        .clamp(1, MAX_FIND_DUPLICATES_LIMIT) as usize;
    let fetch_limit = (limit as i32).saturating_mul(FIND_DUPLICATES_FETCH_MULTIPLIER);
    let include_same_repo = params.include_same_repo.unwrap_or(false);
    debug!(
        tool = "find_duplicates",
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
    let embedding_clusters_truncated = clusters.len() > limit;
    let limited: Vec<_> = clusters.into_iter().take(limit).collect();

    // Shadow-ASR cross-language channel: pull pairs from the
    // `cross_language_signature_clones` materialized table when the
    // cron has populated it. These are symbol-level (not file-level)
    // matches that the embedding-only clustering above does not surface.
    let mut cross_language_pairs: Vec<serde_json::Value> = Vec::new();
    if let Some(pool) = ctx.db().pool()
        && materialized_available(pool).await.unwrap_or(false)
    {
        // P13.3: same articulatory_distance enrichment as
        // tool_find_similar_modules — bring back symbol names so
        // duplicate-pair ranking accounts for phonetic similarity
        // alongside type-signature similarity.
        type ClonePair = (i64, i64, String, String, f32, String, String);
        let rows: Vec<ClonePair> = sqlx::query_as::<_, ClonePair>(
            "SELECT c.symbol_id_a, c.symbol_id_b, c.language_a, c.language_b, c.similarity,
                    fs_a.name AS name_a, fs_b.name AS name_b
             FROM cross_language_signature_clones c
             JOIN file_symbols fs_a ON fs_a.id = c.symbol_id_a
             JOIN file_symbols fs_b ON fs_b.id = c.symbol_id_b
             JOIN indexed_files f_a
               ON f_a.id = fs_a.file_id
              AND f_a.project_id = c.project_id_a
             JOIN indexed_files f_b
               ON f_b.id = fs_b.file_id
              AND f_b.project_id = c.project_id_b
             JOIN projects p_a ON p_a.id = c.project_id_a
             JOIN projects p_b ON p_b.id = c.project_id_b
             WHERE c.similarity >= $1::real
               AND ($2::text IS NULL OR c.language_a = $2 OR c.language_b = $2)
               AND ($3::boolean OR NOT (
                    (p_a.git_common_dir IS NOT NULL AND p_a.git_common_dir = p_b.git_common_dir)
                    OR
                    (p_a.git_root_commits IS NOT NULL AND p_a.git_root_commits = p_b.git_root_commits)
               ))
             ORDER BY c.similarity DESC
             LIMIT $4",
        )
        .bind(min_sim as f32)
        .bind(language.as_deref())
        .bind(include_same_repo)
        .bind(fetch_limit as i64)
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        let dup_cfg = ctx.config().load();
        let merge_threshold = dup_cfg.fuzzy.phonetic_merge_threshold;
        let art_weights = dup_cfg.fuzzy.articulatory_weights();
        for (a, b, lang_a, lang_b, sim, name_a, name_b) in rows {
            let art_dist = crate::fuzzy::phonetic::articulatory_distance_score_weighted(
                &name_a.to_lowercase(),
                &name_b.to_lowercase(),
                &art_weights,
            );
            // Mark pairs that the [fuzzy] cost model would
            // consider "near-name" duplicates (articulatory
            // distance ≤ phonetic_merge_threshold). Consumers can
            // filter on this to find the strongest cross-language
            // duplicate evidence.
            let near_name = art_dist <= merge_threshold;
            cross_language_pairs.push(json!({
                "symbol_id_a": a,
                "symbol_id_b": b,
                "language_a": lang_a,
                "language_b": lang_b,
                "similarity": sim,
                "symbol_name_a": name_a,
                "symbol_name_b": name_b,
                "articulatory_distance": art_dist,
                "near_name_match": near_name,
            }));
        }
    }

    // Combined payload: legacy embedding-derived clusters + new
    // shadow-ASR cross-language symbol-pair channel.
    let cross_language_symbol_pairs_truncated = cross_language_pairs.len() >= fetch_limit as usize;
    let payload = json!({
        "filters": {
            "min_similarity": min_sim,
            "min_projects": min_projects,
            "language": language,
            "limit": limit,
            "include_same_repo": include_same_repo,
        },
        "embedding_clusters": limited,
        "embedding_clusters_truncated": embedding_clusters_truncated,
        "cross_language_symbol_pairs": cross_language_pairs,
        "cross_language_symbol_pairs_truncated": cross_language_symbol_pairs_truncated,
    });
    let json = serde_json::to_string_pretty(&payload)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "find_duplicates",
        clusters = limited.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}

fn normalize_min_similarity(raw: Option<f64>) -> Result<f64, McpError> {
    let value = raw.unwrap_or(DEFAULT_FIND_DUPLICATES_MIN_SIMILARITY);
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
    if language.len() > MAX_FIND_DUPLICATES_LANGUAGE_BYTES {
        return Err(McpError::invalid_params(
            format!("language must be at most {MAX_FIND_DUPLICATES_LANGUAGE_BYTES} bytes"),
            None,
        ));
    }
    Ok(Some(language.to_ascii_lowercase()))
}
