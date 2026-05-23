//! `tool_internal_dry` — DRY *within* one file (real-time).
//!
//! Cross-joins `file_chunks` against itself with `c.id < c'.id`, filtering
//! by the requested similarity threshold. Pairs above threshold are unioned
//! into clusters; each cluster of size >= `min_pairs_per_helper` becomes a
//! `extract_function` recommendation with line ranges.
//!
//! Real-time only (no cron dependency). Files are small (~tens of chunks),
//! embeddings already exist; the cross-join is cheap.

#![allow(unused_imports)]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::json;
use tracing::{debug, info};

use crate::context::SystemContext;
use crate::db::queries;
use crate::mcp::server::*;
use crate::mcp::tools::fix_actions::{
    EstimatedEffort, FixAction, PathRange, RecommendedFix, TargetPath,
};
use crate::mcp::tools::fix_helpers::{pool_or_err, propose_function_name};

const EF_SEARCH_DEFAULT: i32 = 100;

pub async fn tool_internal_dry(
    ctx: &SystemContext,
    params: InternalDryParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .internal_dry_scans
        .fetch_add(1, Ordering::Relaxed);

    let min_similarity = params.min_similarity.unwrap_or(0.80).clamp(0.0, 1.0);
    let min_pairs_per_helper = params.min_pairs_per_helper.unwrap_or(2).max(2);

    debug!(
        tool = "internal_dry",
        file = %params.file,
        min_similarity,
        min_pairs_per_helper,
        "MCP tool invoked",
    );

    let pool = pool_or_err(ctx)?;

    // Resolve the file reference (`project:relative_path` or absolute).
    let file_ref = queries::resolve_file_reference(pool, &params.file)
        .await
        .map_err(|e| McpError::internal_error(format!("File-resolve query failed: {}", e), None))?
        .ok_or_else(|| {
            McpError::invalid_params(format!("File not found: {}", params.file), None)
        })?;

    // Pull intra-file pairs above the threshold.
    let pairs = queries::compare_chunks_within_file(
        pool,
        file_ref.file_id,
        min_similarity,
        EF_SEARCH_DEFAULT,
    )
    .await
    .map_err(|e| McpError::internal_error(format!("Intra-file query failed: {}", e), None))?;

    if pairs.is_empty() {
        return Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&json!({
                "file": file_summary(&file_ref),
                "internal_clusters": [],
                "parameters": {
                    "min_similarity": min_similarity,
                    "min_pairs_per_helper": min_pairs_per_helper,
                },
                "note": format!(
                    "No intra-file chunk pairs at similarity >= {:.2}. \
                     File may be too small or the threshold too aggressive.",
                    min_similarity
                ),
            }))
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?,
        )]));
    }

    // Cluster via union-find on chunk_id pairs.
    let clusters = cluster_intra_file_pairs(&pairs, min_pairs_per_helper);

    let mut output_clusters: Vec<serde_json::Value> = Vec::new();
    for cluster in &clusters {
        // Compute average similarity over in-cluster pairs we observed.
        let mut sim_sum = 0.0_f64;
        let mut sim_count = 0_u64;
        for p in &pairs {
            if cluster.chunk_ids.contains(&p.chunk_id_a)
                && cluster.chunk_ids.contains(&p.chunk_id_b)
            {
                sim_sum += p.similarity;
                sim_count += 1;
            }
        }
        let avg_similarity = if sim_count > 0 {
            sim_sum / sim_count as f64
        } else {
            0.0
        };

        // Member rows + LOC totals.
        let mut members_json: Vec<serde_json::Value> = Vec::new();
        let mut loc_total: i64 = 0;
        let mut loc_count: i64 = 0;
        for &cid in &cluster.chunk_ids {
            if let Some((start_line, end_line)) = cluster.range_per_chunk.get(&cid).copied() {
                let lines = (end_line - start_line + 1).max(1) as i64;
                loc_total += lines;
                loc_count += 1;
                members_json.push(json!({
                    "chunk_id": cid,
                    "lines": format!("{}-{}", start_line, end_line),
                }));
            }
        }
        if members_json.is_empty() {
            continue;
        }
        let loc_avg = if loc_count > 0 {
            loc_total as f64 / loc_count as f64
        } else {
            0.0
        };
        let loc_saved_estimate = ((cluster.chunk_ids.len() as i64 - 1) * loc_avg as i64).max(0);

        // Identifier-based keyword extraction over the cluster's chunk content.
        let keywords = extract_identifier_keywords(&cluster.chunk_ids, &cluster.content_per_chunk);
        let proposed_helper_name = propose_function_name(&keywords);

        // Build the recommended_fix.
        let mut fix =
            RecommendedFix::new(FixAction::ExtractFunction, file_ref.project_name.clone())
                .with_confidence(0.65)
                .with_effort(if loc_avg > 80.0 || cluster.chunk_ids.len() >= 4 {
                    EstimatedEffort::Medium
                } else {
                    EstimatedEffort::Small
                });
        for &cid in &cluster.chunk_ids {
            if let Some((start_line, end_line)) = cluster.range_per_chunk.get(&cid).copied() {
                fix = fix.add_location(PathRange {
                    path: file_ref.relative_path.clone(),
                    start_line: start_line.max(1) as u32,
                    end_line: end_line.max(1) as u32,
                });
            }
        }
        fix = fix
            .add_target(TargetPath {
                path: Some(file_ref.relative_path.clone()),
                suggested_name: Some(proposed_helper_name.clone()),
                ..Default::default()
            })
            .add_step(format!(
                "Extract these {} chunks into a private helper `fn {}` in {}. Replace each \
                 occurrence with a call to the helper. Keywords: {:?}.",
                cluster.chunk_ids.len(),
                proposed_helper_name,
                file_ref.relative_path,
                keywords
            ));
        let fix_json = serde_json::to_value(&fix).map_err(|e| {
            McpError::internal_error(format!("Fix serialization failed: {}", e), None)
        })?;

        output_clusters.push(json!({
            "chunks": members_json,
            "avg_similarity": format!("{:.4}", avg_similarity),
            "proposed_helper_name": proposed_helper_name,
            "keywords": keywords,
            "loc_per_chunk_avg": loc_avg.round() as i64,
            "loc_saved_estimate": loc_saved_estimate,
            "recommended_fix": fix_json,
        }));
    }

    // Sort clusters by loc_saved_estimate descending, then chunk_count.
    output_clusters.sort_by(|a, b| {
        b["loc_saved_estimate"]
            .as_i64()
            .unwrap_or(0)
            .cmp(&a["loc_saved_estimate"].as_i64().unwrap_or(0))
            .then_with(|| {
                b["chunks"]
                    .as_array()
                    .map(|x| x.len())
                    .unwrap_or(0)
                    .cmp(&a["chunks"].as_array().map(|x| x.len()).unwrap_or(0))
            })
    });

    // Shadow-ASR channel (Phase D2b): workspace-wide effect distribution.
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
        "file": file_summary(&file_ref),
        "internal_clusters": output_clusters,
        "parameters": {
            "min_similarity": min_similarity,
            "min_pairs_per_helper": min_pairs_per_helper,
        },
    });
    let json_str = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "internal_dry",
        clusters = output_clusters.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json_str)]))
}

// ============================================================================
// Cluster representation + union-find on intra-file pairs
// ============================================================================

#[derive(Debug)]
struct IntraFileCluster {
    chunk_ids: Vec<i64>,
    range_per_chunk: HashMap<i64, (i32, i32)>,
    content_per_chunk: HashMap<i64, String>,
}

fn cluster_intra_file_pairs(
    pairs: &[queries::ChunkPairSimilarity],
    min_size: usize,
) -> Vec<IntraFileCluster> {
    if pairs.is_empty() {
        return Vec::new();
    }

    let mut chunk_ids: Vec<i64> = Vec::new();
    let mut id_to_idx: HashMap<i64, usize> = HashMap::new();
    let mut range_per_chunk: HashMap<i64, (i32, i32)> = HashMap::new();
    let mut content_per_chunk: HashMap<i64, String> = HashMap::new();

    for pair in pairs {
        for (cid, start, end, content) in [
            (
                pair.chunk_id_a,
                pair.start_line_a,
                pair.end_line_a,
                &pair.content_a,
            ),
            (
                pair.chunk_id_b,
                pair.start_line_b,
                pair.end_line_b,
                &pair.content_b,
            ),
        ] {
            if let std::collections::hash_map::Entry::Vacant(e) = id_to_idx.entry(cid) {
                e.insert(chunk_ids.len());
                chunk_ids.push(cid);
            }
            range_per_chunk.entry(cid).or_insert((start, end));
            content_per_chunk
                .entry(cid)
                .or_insert_with(|| content.clone());
        }
    }

    let mut uf = UnionFind::new(chunk_ids.len());
    for pair in pairs {
        let ia = id_to_idx[&pair.chunk_id_a];
        let ib = id_to_idx[&pair.chunk_id_b];
        uf.union(ia, ib);
    }

    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..chunk_ids.len() {
        groups.entry(uf.find(i)).or_default().push(i);
    }

    let mut out: Vec<IntraFileCluster> = Vec::new();
    for (_, members) in groups {
        if members.len() < min_size {
            continue;
        }
        let mut member_ids: Vec<i64> = members.iter().map(|&i| chunk_ids[i]).collect();
        member_ids.sort_unstable();
        let range_subset: HashMap<i64, (i32, i32)> = member_ids
            .iter()
            .filter_map(|cid| range_per_chunk.get(cid).copied().map(|r| (*cid, r)))
            .collect();
        let content_subset: HashMap<i64, String> = member_ids
            .iter()
            .filter_map(|cid| content_per_chunk.get(cid).cloned().map(|c| (*cid, c)))
            .collect();
        out.push(IntraFileCluster {
            chunk_ids: member_ids,
            range_per_chunk: range_subset,
            content_per_chunk: content_subset,
        });
    }

    out.sort_by_key(|c| std::cmp::Reverse(c.chunk_ids.len()));
    out
}

/// Identifier-frequency keyword extraction over a cluster's content.
/// Tokens shorter than 4 characters or starting with a digit are excluded.
/// Top-5 by frequency, lex-tiebroken.
fn extract_identifier_keywords(
    chunk_ids: &[i64],
    content_per_chunk: &HashMap<i64, String>,
) -> Vec<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for cid in chunk_ids {
        let Some(content) = content_per_chunk.get(cid) else {
            continue;
        };
        for tok in content.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
            if tok.len() < 4 || tok.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                continue;
            }
            *counts.entry(tok.to_ascii_lowercase()).or_insert(0) += 1;
        }
    }
    let mut tokens: Vec<(String, usize)> = counts.into_iter().collect();
    tokens.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    tokens.into_iter().take(5).map(|(t, _)| t).collect()
}

fn file_summary(f: &queries::FileReference) -> serde_json::Value {
    json!({
        "path": f.relative_path,
        "absolute_path": f.path,
        "project": f.project_name,
        "language": f.language,
        "line_count": f.line_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::queries::ChunkPairSimilarity;

    fn pair(a: i64, b: i64, sim: f64, content_a: &str, content_b: &str) -> ChunkPairSimilarity {
        ChunkPairSimilarity {
            chunk_id_a: a,
            content_a: content_a.into(),
            start_line_a: (a as i32) * 10,
            end_line_a: (a as i32) * 10 + 5,
            chunk_id_b: b,
            content_b: content_b.into(),
            start_line_b: (b as i32) * 10,
            end_line_b: (b as i32) * 10 + 5,
            similarity: sim,
        }
    }

    #[test]
    fn cluster_intra_file_groups_transitively() {
        let pairs = vec![
            pair(
                1,
                2,
                0.85,
                "build_request_headers",
                "build_request_headers_v2",
            ),
            pair(
                2,
                3,
                0.83,
                "build_request_headers_v2",
                "build_request_headers_alt",
            ),
        ];
        let clusters = cluster_intra_file_pairs(&pairs, 2);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].chunk_ids, vec![1, 2, 3]);
    }

    #[test]
    fn cluster_intra_file_filters_below_min_size() {
        let pairs = vec![pair(1, 2, 0.85, "x", "y")];
        let clusters = cluster_intra_file_pairs(&pairs, 3);
        assert!(clusters.is_empty(), "min_size=3 rejects 2-chunk groups");
    }

    #[test]
    fn cluster_intra_file_emits_two_groups_when_disjoint() {
        let pairs = vec![
            pair(1, 2, 0.9, "a", "a"),
            pair(2, 3, 0.9, "a", "a"),
            pair(10, 11, 0.9, "b", "b"),
            pair(11, 12, 0.9, "b", "b"),
        ];
        let clusters = cluster_intra_file_pairs(&pairs, 2);
        assert_eq!(clusters.len(), 2);
    }

    #[test]
    fn cluster_intra_file_preserves_line_ranges() {
        let pairs = vec![pair(1, 2, 0.9, "x", "y"), pair(2, 3, 0.9, "y", "z")];
        let clusters = cluster_intra_file_pairs(&pairs, 2);
        assert_eq!(clusters[0].range_per_chunk[&1], (10, 15));
        assert_eq!(clusters[0].range_per_chunk[&2], (20, 25));
        assert_eq!(clusters[0].range_per_chunk[&3], (30, 35));
    }

    #[test]
    fn extract_identifier_keywords_picks_most_frequent() {
        let mut content = HashMap::new();
        content.insert(1, "build_request_headers; auth_token".into());
        content.insert(2, "build_request_headers; build_request_headers".into());
        let kws = extract_identifier_keywords(&[1, 2], &content);
        // `build_request_headers` appears 3x, `auth_token` 1x → first.
        assert_eq!(
            kws.first().map(String::as_str),
            Some("build_request_headers")
        );
    }

    #[test]
    fn extract_identifier_keywords_drops_short_tokens() {
        let mut content = HashMap::new();
        content.insert(1, "ab cd ef gh build".into());
        let kws = extract_identifier_keywords(&[1], &content);
        assert_eq!(kws, vec!["build".to_string()]);
    }

    #[test]
    fn extract_identifier_keywords_empty_input_returns_empty() {
        let content = HashMap::new();
        let kws = extract_identifier_keywords(&[], &content);
        assert!(kws.is_empty());
    }
}
