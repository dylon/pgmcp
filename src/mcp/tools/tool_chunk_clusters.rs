//! `tool_chunk_clusters` — chunk-level DRY clusters across projects.
//!
//! Reads `cross_project_similarities` (chunk-pair table populated by the
//! 6-hour similarity-scan cron), groups chunks via union-find, and emits
//! one cluster row per group with a typed `RecommendedFix` proposing a
//! shared function name + module.
//!
//! Distinct from `find_duplicates` (file-level grouping) and
//! `refactoring_report` (whole-crate extraction). Two files can be 90%
//! different while sharing a small embedded helper — file-level grouping
//! misses that; chunk-level catches it.

#![allow(unused_imports)]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, LoggingLevel};
use serde_json::json;
use tracing::{debug, error, info, warn};

use crate::context::SystemContext;
use crate::db::queries;
use crate::mcp::server::*;
use crate::mcp::tools::fix_actions::{
    EstimatedEffort, FixAction, LocationRef, PathRange, RecommendedFix, TargetPath,
};
use crate::mcp::tools::fix_helpers::{
    infer_module_name_from_topics, pool_or_err, propose_function_name,
};

pub async fn tool_chunk_clusters(
    ctx: &SystemContext,
    params: ChunkClustersParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .chunk_cluster_scans
        .fetch_add(1, Ordering::Relaxed);

    let min_similarity = params.min_similarity.unwrap_or(0.88).clamp(0.0, 1.0);
    let min_cluster_size = params.min_cluster_size.unwrap_or(3).max(2);
    let min_projects = params.min_projects.unwrap_or(2);
    let limit = params.limit.unwrap_or(20).max(1);
    let include_same_repo = params.include_same_repo.unwrap_or(false);
    let worktree_filter = params.worktree_filter.as_deref().unwrap_or("main");
    let main_only = matches!(worktree_filter, "main");

    debug!(
        tool = "chunk_clusters",
        min_similarity,
        min_cluster_size,
        min_projects,
        worktree_filter,
        language = params.language.as_deref().unwrap_or("*"),
        project = params.project.as_deref().unwrap_or("*"),
        "MCP tool invoked",
    );

    let pool = pool_or_err(ctx)?;

    // Resolve main-only project filter once. An empty list is a sentinel
    // meaning "no filter" inside the SQL.
    let main_ids: Vec<i32> = if main_only {
        queries::select_main_worktree_projects(pool)
            .await
            .map_err(|e| {
                McpError::internal_error(format!("Worktree resolver failed: {}", e), None)
            })?
    } else {
        Vec::new()
    };

    // Pull ~5x the requested cluster cap in raw pairs — clustering trims it.
    let raw_pair_cap = (limit as usize).saturating_mul(5).max(50) as i32;
    let pairs = queries::find_chunk_similarity_pairs(
        pool,
        min_similarity,
        params.language.as_deref(),
        &main_ids,
        params.project.as_deref(),
        include_same_repo,
        raw_pair_cap,
    )
    .await
    .map_err(|e| McpError::internal_error(format!("Chunk-pair query failed: {}", e), None))?;

    if pairs.is_empty() {
        return Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&json!({
                "clusters": [],
                "total_clusters": 0,
                "parameters": parameters_echo(&params, min_similarity, min_cluster_size, min_projects, limit, worktree_filter, include_same_repo),
                "guidance": "No chunk-similarity pairs above threshold. \
                             Either threshold is too aggressive, or the similarity-scan cron \
                             hasn't run yet — check `index_stats.similarity_scans`.",
                "health": health_envelope(false, false, false, false, false, false),
            }))
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?,
        )]));
    }

    // Union-find on chunk-id pairs.
    let clusters = cluster_chunk_pairs(&pairs, min_cluster_size, min_projects);
    debug!(
        raw_pairs = pairs.len(),
        clusters = clusters.len(),
        "clustering complete"
    );

    if clusters.is_empty() {
        return Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&json!({
                "clusters": [],
                "total_clusters": 0,
                "parameters": parameters_echo(&params, min_similarity, min_cluster_size, min_projects, limit, worktree_filter, include_same_repo),
                "guidance": format!(
                    "Found {} similarity pairs but no clusters of size >= {} spanning >= {} projects. \
                     Try lowering min_cluster_size or min_projects.",
                    pairs.len(), min_cluster_size, min_projects
                ),
                "health": health_envelope(true, true, false, false, false, false),
            }))
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?,
        )]));
    }

    // Truncate to the limit before fetching content/topics — saves DB work.
    let surviving: Vec<&ClusterCandidate> = clusters.iter().take(limit as usize).collect();

    // Fetch chunk content + line ranges for the surviving clusters in one batch.
    let mut all_chunk_ids: Vec<i64> = Vec::new();
    for c in &surviving {
        all_chunk_ids.extend_from_slice(&c.chunk_ids);
    }
    all_chunk_ids.sort_unstable();
    all_chunk_ids.dedup();

    let content_rows = queries::get_chunk_content_rows(pool, &all_chunk_ids)
        .await
        .map_err(|e| {
            McpError::internal_error(format!("Chunk content query failed: {}", e), None)
        })?;
    let content_by_id: HashMap<i64, queries::ChunkContentRow> =
        content_rows.into_iter().map(|r| (r.chunk_id, r)).collect();

    // Fetch topic summaries for keyword extraction. May be empty if topics
    // haven't been computed; caller falls back to identifier heuristics.
    let topic_rows = queries::get_chunk_topic_summaries(pool, &all_chunk_ids)
        .await
        .map_err(|e| {
            McpError::internal_error(format!("Topic summary query failed: {}", e), None)
        })?;
    let topics_present_anywhere = !topic_rows.is_empty();
    let mut topics_by_chunk: HashMap<i64, Vec<queries::ChunkTopicSummaryRow>> = HashMap::new();
    for row in topic_rows {
        topics_by_chunk.entry(row.chunk_id).or_default().push(row);
    }

    // Emit one JSON record per cluster.
    let mut output_clusters: Vec<serde_json::Value> = Vec::new();
    for cluster in &surviving {
        // Member rows with content where available; skip missing IDs (FK drift).
        let mut members: Vec<serde_json::Value> = Vec::new();
        let mut total_loc: i64 = 0;
        let mut loc_count: i64 = 0;
        for &cid in &cluster.chunk_ids {
            if let Some(content) = content_by_id.get(&cid) {
                let lines = (content.end_line - content.start_line + 1).max(1) as i64;
                total_loc += lines;
                loc_count += 1;
                let project = cluster
                    .project_per_chunk
                    .get(&cid)
                    .cloned()
                    .unwrap_or_default();
                let path = cluster
                    .path_per_chunk
                    .get(&cid)
                    .cloned()
                    .unwrap_or_default();
                members.push(json!({
                    "chunk_id": cid,
                    "file": path,
                    "lines": format!("{}-{}", content.start_line, content.end_line),
                    "project": project,
                }));
            }
        }
        if members.len() < min_cluster_size {
            // FK drift removed too many members; skip this cluster.
            continue;
        }
        let loc_avg = if loc_count > 0 {
            total_loc as f64 / loc_count as f64
        } else {
            0.0
        };
        let loc_saved_estimate = ((cluster.chunk_ids.len() as i64 - 1) * loc_avg as i64).max(0);

        // Aggregate keywords: pick the most-common topic across members, prefer
        // its keyword list. If no topics present, fall back to identifier scan
        // over the centroid chunk's content.
        let (keywords, label_topic) =
            aggregate_cluster_keywords(&cluster.chunk_ids, &topics_by_chunk, &content_by_id);
        let proposed_function_name = propose_function_name(&keywords);
        let proposed_module = infer_module_name_from_topics(&keywords);
        let priority_score = loc_avg
            * cluster.project_count as f64
            * (cluster.chunk_ids.len().saturating_sub(1) as f64);

        // Build the centroid snippet — the chunk closest to the cluster
        // average length is a cheap proxy for "representative."
        let centroid_chunk_id = pick_representative_chunk(&cluster.chunk_ids, &content_by_id);
        let centroid_snippet = centroid_chunk_id
            .and_then(|cid| content_by_id.get(&cid))
            .map(|c| truncate_for_display(&c.content, 240))
            .unwrap_or_default();

        // Build a `RecommendedFix`. Single-project clusters get
        // extract_function (privately scoped); multi-project get extract_module.
        let action = if cluster.project_count >= 2 {
            FixAction::ExtractModule
        } else {
            FixAction::ExtractFunction
        };
        let project_for_fix = cluster
            .first_project_name
            .clone()
            .unwrap_or_else(|| "unknown".into());
        let mut fix = RecommendedFix::new(action, project_for_fix.clone())
            .with_confidence(0.55)
            .with_effort(if loc_avg > 100.0 || cluster.project_count >= 4 {
                EstimatedEffort::Large
            } else if loc_avg > 30.0 || cluster.project_count >= 2 {
                EstimatedEffort::Medium
            } else {
                EstimatedEffort::Small
            });
        for &cid in &cluster.chunk_ids {
            if let Some(content) = content_by_id.get(&cid) {
                let path = cluster
                    .path_per_chunk
                    .get(&cid)
                    .cloned()
                    .unwrap_or_default();
                fix = fix.add_location(PathRange {
                    path,
                    start_line: content.start_line.max(1) as u32,
                    end_line: content.end_line.max(1) as u32,
                });
            }
        }
        match action {
            FixAction::ExtractModule => {
                fix = fix
                    .add_target(TargetPath {
                        suggested_new_path: Some(format!("shared/{}/lib.rs", proposed_module)),
                        suggested_name: Some(proposed_function_name.clone()),
                        ..Default::default()
                    })
                    .add_step(format!(
                        "Extract these {} chunks (avg ~{:.0} LOC each, spanning {} projects) into a \
                         new shared crate `{}` exposing fn `{}`. Replace each call site with an \
                         import from the new crate.",
                        cluster.chunk_ids.len(),
                        loc_avg,
                        cluster.project_count,
                        proposed_module,
                        proposed_function_name,
                    ));
            }
            FixAction::ExtractFunction => {
                fix = fix
                    .add_target(TargetPath {
                        suggested_name: Some(proposed_function_name.clone()),
                        ..Default::default()
                    })
                    .add_step(format!(
                        "Extract these {} chunks into a single function `{}` and replace duplicates \
                         with calls. Topic keywords: {:?}.",
                        cluster.chunk_ids.len(),
                        proposed_function_name,
                        keywords
                    ));
            }
            _ => {}
        }
        let fix_json = serde_json::to_value(&fix).map_err(|e| {
            McpError::internal_error(format!("Fix serialization failed: {}", e), None)
        })?;

        output_clusters.push(json!({
            "cluster_id": format!("ck_{}", cluster.chunk_ids.first().copied().unwrap_or(0)),
            "chunk_count": cluster.chunk_ids.len(),
            "project_count": cluster.project_count,
            "projects": cluster.project_names.iter().collect::<Vec<_>>(),
            "language": cluster.language.clone(),
            "avg_similarity": format!("{:.4}", cluster.avg_similarity),
            "loc_per_chunk_avg": loc_avg.round() as i64,
            "loc_saved_estimate": loc_saved_estimate,
            "proposed_function_name": proposed_function_name,
            "proposed_module": proposed_module,
            "keywords": keywords,
            "label_topic": label_topic,
            "centroid_snippet": centroid_snippet,
            "members": members,
            "recommended_fix": fix_json,
            "priority_score": format!("{:.2}", priority_score),
        }));
    }

    // Sort by priority descending, then chunk_count, then cluster_id (stable).
    output_clusters.sort_by(|a, b| {
        let pa = a["priority_score"]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        let pb = b["priority_score"]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        pb.partial_cmp(&pa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                b["chunk_count"]
                    .as_u64()
                    .unwrap_or(0)
                    .cmp(&a["chunk_count"].as_u64().unwrap_or(0))
            })
    });

    let total = output_clusters.len();
    let result = json!({
        "clusters": output_clusters,
        "total_clusters": total,
        "parameters": parameters_echo(&params, min_similarity, min_cluster_size, min_projects, limit, worktree_filter, include_same_repo),
        "guidance": format!(
            "Top {} clusters ranked by loc_saved × project_count. Each cluster carries a typed \
             `recommended_fix` for direct dispatch by another agent. extract_module candidates \
             span ≥2 projects; extract_function candidates are single-project.",
            total
        ),
        "health": health_envelope(true, true, topics_present_anywhere, false, false, false),
    });
    let json_str = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "chunk_clusters",
        total_clusters = total,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json_str)]))
}

// ============================================================================
// Cluster representation
// ============================================================================

#[derive(Debug)]
struct ClusterCandidate {
    /// Member chunk_ids sorted ascending (stable for output).
    chunk_ids: Vec<i64>,
    /// Distinct project_ids in the cluster.
    project_count: usize,
    /// Distinct project names (for echo).
    project_names: HashSet<String>,
    /// Average pairwise similarity across in-cluster pairs.
    avg_similarity: f64,
    /// Language of the cluster (homogeneous within a cluster).
    language: String,
    /// Path lookup for each member chunk_id.
    path_per_chunk: HashMap<i64, String>,
    /// Project lookup for each member chunk_id.
    project_per_chunk: HashMap<i64, String>,
    /// First-encountered project name (used as `RecommendedFix.location.project`).
    first_project_name: Option<String>,
}

/// Group `ChunkSimilarityPair` rows by union-find on chunk_ids. Returns
/// clusters of size >= min_cluster_size that span >= min_projects projects,
/// sorted by descending pair count.
fn cluster_chunk_pairs(
    pairs: &[queries::ChunkSimilarityPair],
    min_cluster_size: usize,
    min_projects: usize,
) -> Vec<ClusterCandidate> {
    if pairs.is_empty() {
        return Vec::new();
    }

    // Stable index assignment for the union-find.
    let mut chunk_ids: Vec<i64> = Vec::new();
    let mut id_to_idx: HashMap<i64, usize> = HashMap::new();
    let mut path_per_chunk: HashMap<i64, String> = HashMap::new();
    let mut project_per_chunk: HashMap<i64, String> = HashMap::new();
    let mut project_id_per_chunk: HashMap<i64, i32> = HashMap::new();
    let mut language_per_pair: Vec<&str> = Vec::with_capacity(pairs.len());

    for pair in pairs {
        for (cid, fid_path, pname, pid) in [
            (
                pair.chunk_id_a,
                &pair.path_a,
                &pair.project_name_a,
                pair.project_id_a,
            ),
            (
                pair.chunk_id_b,
                &pair.path_b,
                &pair.project_name_b,
                pair.project_id_b,
            ),
        ] {
            if let std::collections::hash_map::Entry::Vacant(e) = id_to_idx.entry(cid) {
                e.insert(chunk_ids.len());
                chunk_ids.push(cid);
            }
            path_per_chunk
                .entry(cid)
                .or_insert_with(|| fid_path.clone());
            project_per_chunk
                .entry(cid)
                .or_insert_with(|| pname.clone());
            project_id_per_chunk.entry(cid).or_insert(pid);
        }
        language_per_pair.push(pair.language.as_str());
    }

    // Union-find using the existing UnionFind helper.
    let mut uf = UnionFind::new(chunk_ids.len());
    let mut pair_sims: HashMap<(usize, usize), f64> = HashMap::new();
    for pair in pairs {
        let ia = id_to_idx[&pair.chunk_id_a];
        let ib = id_to_idx[&pair.chunk_id_b];
        uf.union(ia, ib);
        pair_sims.insert((ia.min(ib), ia.max(ib)), pair.similarity);
    }

    // Collect roots → member indices.
    let mut clusters_idx: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..chunk_ids.len() {
        clusters_idx.entry(uf.find(i)).or_default().push(i);
    }

    let mut out: Vec<ClusterCandidate> = Vec::new();
    for (_, members) in clusters_idx {
        if members.len() < min_cluster_size {
            continue;
        }

        // Project-uniqueness check.
        let mut project_ids: HashSet<i32> = HashSet::new();
        let mut project_names: HashSet<String> = HashSet::new();
        let mut first_project: Option<String> = None;
        for &idx in &members {
            let cid = chunk_ids[idx];
            if let Some(pid) = project_id_per_chunk.get(&cid).copied() {
                project_ids.insert(pid);
            }
            if let Some(name) = project_per_chunk.get(&cid) {
                if first_project.is_none() {
                    first_project = Some(name.clone());
                }
                project_names.insert(name.clone());
            }
        }
        if project_ids.len() < min_projects {
            continue;
        }

        // Average pairwise similarity across member pairs that the materialized table covers.
        let mut sim_sum = 0.0f64;
        let mut sim_count = 0u64;
        for i in 0..members.len() {
            for j in (i + 1)..members.len() {
                let key = (members[i].min(members[j]), members[i].max(members[j]));
                if let Some(&s) = pair_sims.get(&key) {
                    sim_sum += s;
                    sim_count += 1;
                }
            }
        }
        let avg_similarity = if sim_count > 0 {
            sim_sum / sim_count as f64
        } else {
            0.0
        };

        // Language: take the first pair's language seen for this cluster.
        let language = pairs
            .iter()
            .find(|p| {
                id_to_idx
                    .get(&p.chunk_id_a)
                    .map(|i| members.contains(i))
                    .unwrap_or(false)
            })
            .map(|p| p.language.clone())
            .unwrap_or_else(|| "unknown".to_string());

        // Member chunk_ids sorted ascending.
        let mut member_ids: Vec<i64> = members.iter().map(|&i| chunk_ids[i]).collect();
        member_ids.sort_unstable();

        let path_subset: HashMap<i64, String> = member_ids
            .iter()
            .filter_map(|cid| path_per_chunk.get(cid).cloned().map(|p| (*cid, p)))
            .collect();
        let project_subset: HashMap<i64, String> = member_ids
            .iter()
            .filter_map(|cid| project_per_chunk.get(cid).cloned().map(|p| (*cid, p)))
            .collect();

        out.push(ClusterCandidate {
            chunk_ids: member_ids,
            project_count: project_ids.len(),
            project_names,
            avg_similarity,
            language,
            path_per_chunk: path_subset,
            project_per_chunk: project_subset,
            first_project_name: first_project,
        });
    }

    // Sort: larger cluster first (more savings), then more projects, then higher similarity.
    out.sort_by(|a, b| {
        b.chunk_ids
            .len()
            .cmp(&a.chunk_ids.len())
            .then_with(|| b.project_count.cmp(&a.project_count))
            .then_with(|| {
                b.avg_similarity
                    .partial_cmp(&a.avg_similarity)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| {
                a.chunk_ids
                    .first()
                    .copied()
                    .unwrap_or(0)
                    .cmp(&b.chunk_ids.first().copied().unwrap_or(0))
            })
    });

    out
}

// ============================================================================
// Helpers
// ============================================================================

/// Aggregate keywords across cluster chunks. Picks the most-common topic
/// among members; returns its keyword list and label. Falls back to a
/// regex-derived identifier list if no topic data is present.
fn aggregate_cluster_keywords(
    chunk_ids: &[i64],
    topics_by_chunk: &HashMap<i64, Vec<queries::ChunkTopicSummaryRow>>,
    content_by_id: &HashMap<i64, queries::ChunkContentRow>,
) -> (Vec<String>, Option<String>) {
    // Per-topic accumulator: (member_count, label, keyword_list, summed_membership).
    type TopicVote = (usize, String, Option<Vec<String>>, f64);

    let mut counts: HashMap<i64, TopicVote> = HashMap::new();
    for &cid in chunk_ids {
        if let Some(rows) = topics_by_chunk.get(&cid) {
            for r in rows {
                let entry = counts.entry(r.topic_id).or_insert((
                    0,
                    r.label.clone(),
                    r.keywords.clone(),
                    0.0,
                ));
                entry.0 += 1;
                entry.3 += r.membership_score;
            }
        }
    }
    if let Some((_, label, keywords, _)) = counts.into_values().max_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.3.partial_cmp(&b.3).unwrap_or(std::cmp::Ordering::Equal))
    }) {
        let kws = keywords.unwrap_or_default();
        if !kws.is_empty() {
            return (kws.into_iter().take(5).collect(), Some(label));
        }
    }

    // Topic data unavailable: extract identifier-like tokens from the
    // representative chunk's content and pick the most-frequent.
    let representative = pick_representative_chunk(chunk_ids, content_by_id);
    let content = representative
        .and_then(|cid| content_by_id.get(&cid))
        .map(|c| c.content.as_str())
        .unwrap_or("");
    let mut tok_counts: HashMap<String, usize> = HashMap::new();
    for tok in content.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
        if tok.len() < 4 || tok.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            continue;
        }
        *tok_counts.entry(tok.to_ascii_lowercase()).or_insert(0) += 1;
    }
    let mut tokens: Vec<(String, usize)> = tok_counts.into_iter().collect();
    tokens.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let kws: Vec<String> = tokens.into_iter().take(5).map(|(t, _)| t).collect();
    (kws, None)
}

/// Pick the chunk with content length closest to the median — a cheap
/// "most representative" heuristic when we don't want to compute centroids
/// over embeddings.
fn pick_representative_chunk(
    chunk_ids: &[i64],
    content_by_id: &HashMap<i64, queries::ChunkContentRow>,
) -> Option<i64> {
    let mut lengths: Vec<(i64, usize)> = chunk_ids
        .iter()
        .filter_map(|cid| content_by_id.get(cid).map(|c| (*cid, c.content.len())))
        .collect();
    if lengths.is_empty() {
        return None;
    }
    lengths.sort_by_key(|(_, len)| *len);
    let mid = lengths.len() / 2;
    Some(lengths[mid].0)
}

fn truncate_for_display(s: &str, max_chars: usize) -> String {
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

fn parameters_echo(
    params: &ChunkClustersParams,
    min_similarity: f64,
    min_cluster_size: usize,
    min_projects: usize,
    limit: i32,
    worktree_filter: &str,
    include_same_repo: bool,
) -> serde_json::Value {
    json!({
        "min_similarity": min_similarity,
        "min_cluster_size": min_cluster_size,
        "min_projects": min_projects,
        "limit": limit,
        "language": params.language,
        "project": params.project,
        "worktree_filter": worktree_filter,
        "include_same_repo": include_same_repo,
    })
}

fn health_envelope(
    similarity_present: bool,
    graph_present: bool,
    topics_present: bool,
    blame_present: bool,
    git_history_present: bool,
    symbols_present: bool,
) -> serde_json::Value {
    json!({
        "similarity_stale": !similarity_present,
        "graph_stale": !graph_present,
        "topics_present": topics_present,
        "blame_present": blame_present,
        "git_history_present": git_history_present,
        "symbols_present": symbols_present,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::queries::ChunkSimilarityPair;

    fn pair(a: i64, b: i64, proj_a: i32, proj_b: i32, sim: f64) -> ChunkSimilarityPair {
        ChunkSimilarityPair {
            chunk_id_a: a,
            chunk_id_b: b,
            file_id_a: 100 + a,
            file_id_b: 100 + b,
            path_a: format!("p{}/file_{}.rs", proj_a, a),
            path_b: format!("p{}/file_{}.rs", proj_b, b),
            project_id_a: proj_a,
            project_id_b: proj_b,
            project_name_a: format!("project_{}", proj_a),
            project_name_b: format!("project_{}", proj_b),
            language: "rust".into(),
            similarity: sim,
        }
    }

    #[test]
    fn cluster_chunk_pairs_groups_transitively_connected_chunks() {
        // Chain: 1-2 (proj 10/20), 2-3 (proj 20/30), 3-4 (proj 30/40)
        // → all four chunks in one cluster spanning 4 projects.
        let pairs = vec![
            pair(1, 2, 10, 20, 0.92),
            pair(2, 3, 20, 30, 0.91),
            pair(3, 4, 30, 40, 0.90),
        ];
        let clusters = cluster_chunk_pairs(&pairs, 3, 2);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].chunk_ids, vec![1, 2, 3, 4]);
        assert_eq!(clusters[0].project_count, 4);
    }

    #[test]
    fn cluster_chunk_pairs_filters_below_min_cluster_size() {
        // Three pairs but only two distinct chunks each → single 2-chunk cluster.
        let pairs = vec![pair(1, 2, 10, 20, 0.9), pair(3, 4, 30, 40, 0.9)];
        let clusters = cluster_chunk_pairs(&pairs, 3, 2);
        assert!(
            clusters.is_empty(),
            "min_cluster_size=3 should reject 2-chunk pairs"
        );
    }

    #[test]
    fn cluster_chunk_pairs_filters_below_min_projects() {
        // Cluster of 3 chunks but only 1 project → fails min_projects=2.
        let pairs = vec![pair(1, 2, 10, 10, 0.95), pair(2, 3, 10, 10, 0.94)];
        let clusters = cluster_chunk_pairs(&pairs, 3, 2);
        assert!(
            clusters.is_empty(),
            "single-project clusters fail min_projects=2"
        );
    }

    #[test]
    fn cluster_chunk_pairs_computes_avg_similarity_over_seen_pairs() {
        // 1-2 sim 0.90, 2-3 sim 0.95, 1-3 not in materialized → use only seen.
        let pairs = vec![pair(1, 2, 10, 20, 0.90), pair(2, 3, 20, 30, 0.95)];
        let clusters = cluster_chunk_pairs(&pairs, 3, 2);
        assert_eq!(clusters.len(), 1);
        let avg = (0.90 + 0.95) / 2.0;
        assert!(
            (clusters[0].avg_similarity - avg).abs() < 1e-9,
            "got {} expected {}",
            clusters[0].avg_similarity,
            avg
        );
    }

    #[test]
    fn cluster_chunk_pairs_preserves_path_and_project_metadata() {
        let pairs = vec![pair(1, 2, 10, 20, 0.9), pair(2, 3, 20, 30, 0.9)];
        let clusters = cluster_chunk_pairs(&pairs, 3, 2);
        assert_eq!(clusters.len(), 1);
        let c = &clusters[0];
        assert_eq!(c.path_per_chunk[&1], "p10/file_1.rs");
        assert_eq!(c.project_per_chunk[&3], "project_30");
    }

    #[test]
    fn cluster_chunk_pairs_sorts_larger_clusters_first() {
        // Cluster A: 1-2-3 (3 chunks, 3 projects)
        // Cluster B: 10-11 (2 chunks, 2 projects) — fails min_cluster_size=3
        // Cluster C: 20-21-22-23 (4 chunks, 4 projects)
        let pairs = vec![
            pair(1, 2, 10, 20, 0.9),
            pair(2, 3, 20, 30, 0.9),
            pair(10, 11, 100, 200, 0.9),
            pair(20, 21, 30, 40, 0.9),
            pair(21, 22, 40, 50, 0.9),
            pair(22, 23, 50, 60, 0.9),
        ];
        let clusters = cluster_chunk_pairs(&pairs, 3, 2);
        assert_eq!(clusters.len(), 2);
        assert_eq!(clusters[0].chunk_ids.len(), 4, "larger cluster first");
        assert_eq!(clusters[1].chunk_ids.len(), 3);
    }

    #[test]
    fn cluster_chunk_pairs_empty_input_yields_empty_output() {
        let clusters = cluster_chunk_pairs(&[], 3, 2);
        assert!(clusters.is_empty());
    }

    #[test]
    fn pick_representative_chunk_picks_median_length() {
        let mut content = HashMap::new();
        content.insert(
            1,
            queries::ChunkContentRow {
                chunk_id: 1,
                file_id: 1,
                start_line: 1,
                end_line: 10,
                content: "a".repeat(50),
            },
        );
        content.insert(
            2,
            queries::ChunkContentRow {
                chunk_id: 2,
                file_id: 2,
                start_line: 1,
                end_line: 10,
                content: "b".repeat(100),
            },
        );
        content.insert(
            3,
            queries::ChunkContentRow {
                chunk_id: 3,
                file_id: 3,
                start_line: 1,
                end_line: 10,
                content: "c".repeat(200),
            },
        );
        let rep = pick_representative_chunk(&[1, 2, 3], &content);
        // Median length = 100 → chunk_id 2.
        assert_eq!(rep, Some(2));
    }

    #[test]
    fn truncate_for_display_appends_ellipsis_when_truncated() {
        let s = "abcdefghij";
        assert_eq!(truncate_for_display(s, 10), "abcdefghij");
        assert_eq!(truncate_for_display(s, 5), "abcde…");
    }
}
