//! `tool_boilerplate_clusters` — codegen-worthy near-identical chunks.
//!
//! Builds on the same `find_chunk_similarity_pairs` query as `chunk_clusters`,
//! but with a much higher default similarity threshold (0.96). After
//! clustering, each cluster's chunk content is fetched, identifiers are
//! normalized to positional placeholders (`__ID0__`, `__ID1__`, …), and
//! the cluster qualifies only if the resulting Jaccard similarity is
//! ≥ `min_normalized_match` (default 0.99) — i.e. chunks differ *only* in
//! identifier names.
//!
//! These are macro / generic / template candidates: code that exists
//! mechanically across multiple instances and can be parameterized.

#![allow(unused_imports)]

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use std::sync::atomic::Ordering;
use std::time::Instant;

use regex::Regex;
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
use crate::mcp::tools::fix_helpers::pool_or_err;

pub async fn tool_boilerplate_clusters(
    ctx: &SystemContext,
    params: BoilerplateClustersParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .boilerplate_scans
        .fetch_add(1, Ordering::Relaxed);

    let min_similarity = params.min_similarity.unwrap_or(0.96).clamp(0.0, 1.0);
    let min_cluster_size = params.min_cluster_size.unwrap_or(3).max(2);
    let min_normalized_match = params.min_normalized_match.unwrap_or(0.99).clamp(0.0, 1.0);
    let limit = params.limit.unwrap_or(20).max(1);
    let include_same_repo = params.include_same_repo.unwrap_or(false);
    let worktree_filter = params.worktree_filter.as_deref().unwrap_or("main");
    let main_only = matches!(worktree_filter, "main");

    debug!(
        tool = "boilerplate_clusters",
        min_similarity, min_cluster_size, min_normalized_match, worktree_filter, "MCP tool invoked",
    );

    let pool = pool_or_err(ctx)?;

    let main_ids: Vec<i32> = if main_only {
        queries::select_main_worktree_projects(pool)
            .await
            .map_err(|e| {
                McpError::internal_error(format!("Worktree resolver failed: {}", e), None)
            })?
    } else {
        Vec::new()
    };

    let raw_pair_cap = (limit as usize).saturating_mul(10).max(50) as i32;
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
                "boilerplate_clusters": [],
                "total_clusters": 0,
                "parameters": parameters_echo(&params, min_similarity, min_cluster_size, min_normalized_match, limit, worktree_filter, include_same_repo),
                "note": format!(
                    "No chunk pairs at similarity >= {:.2}. Either no boilerplate exists or the \
                     similarity-scan cron hasn't reached this threshold yet.",
                    min_similarity
                ),
            }))
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?,
        )]));
    }

    // Cluster via union-find.
    let raw_clusters = cluster_chunk_pairs(&pairs, min_cluster_size);
    let surviving: Vec<&RawCluster> = raw_clusters.iter().take(limit as usize * 2).collect();

    // Fetch all chunk content for normalized-match check.
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

    let mut output: Vec<serde_json::Value> = Vec::new();
    for cluster in &surviving {
        // Build per-chunk normalized form + identifier-position map.
        let mut normalized: Vec<(i64, String, Vec<String>)> = Vec::new();
        for &cid in &cluster.chunk_ids {
            let Some(content_row) = content_by_id.get(&cid) else {
                continue;
            };
            let (norm, identifiers) = normalize_identifiers(&content_row.content);
            normalized.push((cid, norm, identifiers));
        }
        if normalized.len() < min_cluster_size {
            continue;
        }

        // Compute pairwise Jaccard similarity over normalized line tokens.
        // The cluster qualifies only if the average across all observed
        // normalized pairs is >= min_normalized_match.
        let avg_normalized = mean_pairwise_jaccard(&normalized);
        if avg_normalized < min_normalized_match {
            continue;
        }

        // Collect differing identifiers position-by-position.
        let differing = collect_differing_identifiers(&normalized);

        // Determine cluster language from one of its chunk paths (via the pair table).
        let language = pairs
            .iter()
            .find(|p| {
                cluster.chunk_ids.contains(&p.chunk_id_a)
                    || cluster.chunk_ids.contains(&p.chunk_id_b)
            })
            .map(|p| p.language.clone())
            .unwrap_or_else(|| "unknown".to_string());

        // Average similarity from the raw pairs that fall inside this cluster.
        let mut sim_sum = 0.0;
        let mut sim_count = 0_u64;
        for p in &pairs {
            if cluster.chunk_ids.contains(&p.chunk_id_a)
                && cluster.chunk_ids.contains(&p.chunk_id_b)
            {
                sim_sum += p.similarity;
                sim_count += 1;
            }
        }
        let raw_avg_similarity = if sim_count > 0 {
            sim_sum / sim_count as f64
        } else {
            0.0
        };

        // Members list with metadata from the pair rows.
        let mut members_json: Vec<serde_json::Value> = Vec::new();
        let mut total_loc: i64 = 0;
        let mut loc_count: i64 = 0;
        for &cid in &cluster.chunk_ids {
            if let Some(content) = content_by_id.get(&cid) {
                let lines = (content.end_line - content.start_line + 1).max(1) as i64;
                total_loc += lines;
                loc_count += 1;
                let path = pair_path_for_chunk(&pairs, cid).unwrap_or_default();
                members_json.push(json!({
                    "chunk_id": cid,
                    "file": path,
                    "lines": format!("{}-{}", content.start_line, content.end_line),
                }));
            }
        }
        let loc_avg = if loc_count > 0 {
            total_loc as f64 / loc_count as f64
        } else {
            0.0
        };
        let loc_saved_estimate = ((cluster.chunk_ids.len() as i64 - 1) * loc_avg as i64).max(0);

        let proposed_abstraction = abstraction_for_language(&language);
        let priority_score =
            loc_avg * (cluster.chunk_ids.len().saturating_sub(1) as f64) * (avg_normalized * 1.0);

        // recommended_fix
        let project_for_fix = pair_project_for_chunk(&pairs, cluster.chunk_ids[0])
            .unwrap_or_else(|| "unknown".to_string());
        let mut fix = RecommendedFix::new(FixAction::ExtractMacro, project_for_fix)
            .with_confidence(0.65)
            .with_effort(if cluster.chunk_ids.len() >= 6 || loc_avg > 60.0 {
                EstimatedEffort::Medium
            } else {
                EstimatedEffort::Small
            });
        for &cid in &cluster.chunk_ids {
            if let Some(content) = content_by_id.get(&cid) {
                let path = pair_path_for_chunk(&pairs, cid).unwrap_or_default();
                fix = fix.add_location(PathRange {
                    path,
                    start_line: content.start_line.max(1) as u32,
                    end_line: content.end_line.max(1) as u32,
                });
            }
        }
        fix = fix.add_step(format!(
            "These {} chunks normalize to identical text (Jaccard {:.2}); the only differences \
             are renamed identifiers ({} positions vary). Replace with {} parameterized over \
             those positions.",
            cluster.chunk_ids.len(),
            avg_normalized,
            differing.len(),
            proposed_abstraction
        ));
        let fix_json = serde_json::to_value(&fix).map_err(|e| {
            McpError::internal_error(format!("Fix serialization failed: {}", e), None)
        })?;

        output.push(json!({
            "cluster_id": format!("bp_{}", cluster.chunk_ids.first().copied().unwrap_or(0)),
            "chunk_count": cluster.chunk_ids.len(),
            "language": language,
            "raw_avg_similarity": format!("{:.4}", raw_avg_similarity),
            "normalized_match_ratio": format!("{:.4}", avg_normalized),
            "differing_identifiers": differing,
            "proposed_abstraction": proposed_abstraction,
            "members": members_json,
            "loc_per_chunk_avg": loc_avg.round() as i64,
            "loc_saved_estimate": loc_saved_estimate,
            "recommended_fix": fix_json,
            "priority_score": format!("{:.2}", priority_score),
        }));
    }

    output.sort_by(|a, b| {
        let pa: f64 = a["priority_score"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        let pb: f64 = b["priority_score"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        pb.partial_cmp(&pa).unwrap_or(std::cmp::Ordering::Equal)
    });
    output.truncate(limit as usize);

    let total = output.len();
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
        "boilerplate_clusters": output,
        "total_clusters": total,
        "parameters": parameters_echo(&params, min_similarity, min_cluster_size, min_normalized_match, limit, worktree_filter, include_same_repo),
        "guidance": format!(
            "Top {} boilerplate clusters. Each was verified by normalizing identifiers and \
             checking that the residue is identical (Jaccard >= {}). recommended_fix is \
             extract_macro; the cluster's `differing_identifiers` show which positional \
             values vary across instances.",
            total, min_normalized_match
        ),
    });
    let json_str = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "boilerplate_clusters",
        clusters = total,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json_str)]))
}

// ============================================================================
// Identifier normalization
// ============================================================================

fn identifier_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[A-Za-z_][A-Za-z0-9_]*").expect("identifier regex"))
}

/// Replace each identifier in `content` with a positional placeholder.
/// Returns the normalized form + the per-position identifier list.
///
/// Position is the order of *first occurrence* of each unique identifier.
/// E.g. `Foo::bar(Foo)` → `__ID0__::__ID1__(__ID0__)` with identifiers
/// `["Foo", "bar"]`.
fn normalize_identifiers(content: &str) -> (String, Vec<String>) {
    let re = identifier_re();
    let mut id_to_pos: HashMap<String, usize> = HashMap::new();
    let mut ordered: Vec<String> = Vec::new();
    let mut out = String::with_capacity(content.len());
    let mut last_end = 0;
    for m in re.find_iter(content) {
        out.push_str(&content[last_end..m.start()]);
        let ident = m.as_str();
        let pos = match id_to_pos.get(ident) {
            Some(&p) => p,
            None => {
                let p = ordered.len();
                id_to_pos.insert(ident.to_string(), p);
                ordered.push(ident.to_string());
                p
            }
        };
        out.push_str(&format!("__ID{}__", pos));
        last_end = m.end();
    }
    out.push_str(&content[last_end..]);
    (out, ordered)
}

/// Mean pairwise Jaccard similarity over normalized strings (line-level).
/// O(n²) in cluster size; fine for clusters up to ~20 chunks.
fn mean_pairwise_jaccard(normalized: &[(i64, String, Vec<String>)]) -> f64 {
    if normalized.len() < 2 {
        return 1.0;
    }
    let mut sum = 0.0;
    let mut count = 0_u64;
    for i in 0..normalized.len() {
        for j in (i + 1)..normalized.len() {
            let a: HashSet<&str> = normalized[i]
                .1
                .lines()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();
            let b: HashSet<&str> = normalized[j]
                .1
                .lines()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();
            let intersection = a.intersection(&b).count();
            let union = a.union(&b).count();
            let jaccard = if union == 0 {
                1.0
            } else {
                intersection as f64 / union as f64
            };
            sum += jaccard;
            count += 1;
        }
    }
    if count == 0 { 1.0 } else { sum / count as f64 }
}

/// At each identifier-position present in any cluster member, gather the
/// distinct values (across all members). Positions where every member uses
/// the same identifier are skipped — they aren't varying.
fn collect_differing_identifiers(
    normalized: &[(i64, String, Vec<String>)],
) -> Vec<serde_json::Value> {
    if normalized.is_empty() {
        return Vec::new();
    }
    let max_pos = normalized
        .iter()
        .map(|(_, _, ids)| ids.len())
        .max()
        .unwrap_or(0);
    let mut out: Vec<serde_json::Value> = Vec::new();
    for pos in 0..max_pos {
        let mut values: Vec<String> = Vec::new();
        let mut distinct: HashSet<&str> = HashSet::new();
        for (_, _, ids) in normalized {
            if let Some(v) = ids.get(pos)
                && distinct.insert(v.as_str())
            {
                values.push(v.clone());
            }
        }
        if distinct.len() > 1 {
            out.push(json!({
                "position": pos,
                "values": values,
            }));
        }
    }
    out
}

fn abstraction_for_language(language: &str) -> &'static str {
    match language {
        "rust" => "macro_rules! / generic",
        "java" | "kotlin" | "csharp" => "generic class / method",
        "typescript" | "javascript" => "generic / template literal type",
        "python" => "TypeVar / Generic class / decorator",
        "c" | "cpp" => "macro / template",
        _ => "parameterized template",
    }
}

// ============================================================================
// Cluster representation (lighter-weight than chunk_clusters' version)
// ============================================================================

#[derive(Debug)]
struct RawCluster {
    chunk_ids: Vec<i64>,
}

fn cluster_chunk_pairs(
    pairs: &[queries::ChunkSimilarityPair],
    min_cluster_size: usize,
) -> Vec<RawCluster> {
    if pairs.is_empty() {
        return Vec::new();
    }
    let mut chunk_ids: Vec<i64> = Vec::new();
    let mut id_to_idx: HashMap<i64, usize> = HashMap::new();
    for pair in pairs {
        for cid in [pair.chunk_id_a, pair.chunk_id_b] {
            if let std::collections::hash_map::Entry::Vacant(e) = id_to_idx.entry(cid) {
                e.insert(chunk_ids.len());
                chunk_ids.push(cid);
            }
        }
    }
    let mut uf = UnionFind::new(chunk_ids.len());
    for pair in pairs {
        uf.union(id_to_idx[&pair.chunk_id_a], id_to_idx[&pair.chunk_id_b]);
    }
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..chunk_ids.len() {
        groups.entry(uf.find(i)).or_default().push(i);
    }
    let mut out: Vec<RawCluster> = Vec::new();
    for (_, members) in groups {
        if members.len() < min_cluster_size {
            continue;
        }
        let mut member_ids: Vec<i64> = members.iter().map(|&i| chunk_ids[i]).collect();
        member_ids.sort_unstable();
        out.push(RawCluster {
            chunk_ids: member_ids,
        });
    }
    out.sort_by_key(|c| std::cmp::Reverse(c.chunk_ids.len()));
    out
}

// ============================================================================
// Helpers — pair-row lookups
// ============================================================================

fn pair_path_for_chunk(pairs: &[queries::ChunkSimilarityPair], cid: i64) -> Option<String> {
    for p in pairs {
        if p.chunk_id_a == cid {
            return Some(p.path_a.clone());
        }
        if p.chunk_id_b == cid {
            return Some(p.path_b.clone());
        }
    }
    None
}

fn pair_project_for_chunk(pairs: &[queries::ChunkSimilarityPair], cid: i64) -> Option<String> {
    for p in pairs {
        if p.chunk_id_a == cid {
            return Some(p.project_name_a.clone());
        }
        if p.chunk_id_b == cid {
            return Some(p.project_name_b.clone());
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn parameters_echo(
    params: &BoilerplateClustersParams,
    min_similarity: f64,
    min_cluster_size: usize,
    min_normalized_match: f64,
    limit: i32,
    worktree_filter: &str,
    include_same_repo: bool,
) -> serde_json::Value {
    json!({
        "min_similarity": min_similarity,
        "min_cluster_size": min_cluster_size,
        "min_normalized_match": min_normalized_match,
        "language": params.language,
        "project": params.project,
        "limit": limit,
        "worktree_filter": worktree_filter,
        "include_same_repo": include_same_repo,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_identifiers_replaces_each_unique_identifier_once() {
        let (norm, ids) = normalize_identifiers("Foo::bar(Foo, baz)");
        assert_eq!(norm, "__ID0__::__ID1__(__ID0__, __ID2__)");
        assert_eq!(ids, vec!["Foo".to_string(), "bar".into(), "baz".into()]);
    }

    #[test]
    fn normalize_identifiers_handles_no_identifiers() {
        let (norm, ids) = normalize_identifiers("();");
        assert_eq!(norm, "();");
        assert!(ids.is_empty());
    }

    #[test]
    fn normalize_identifiers_preserves_punctuation_between_tokens() {
        let (norm, _) = normalize_identifiers("a.b().c");
        // Each identifier replaced; punctuation kept verbatim.
        assert_eq!(norm, "__ID0__.__ID1__().__ID2__");
    }

    #[test]
    fn mean_pairwise_jaccard_singleton_returns_one() {
        let normalized = vec![(1, "abc".to_string(), vec![])];
        assert_eq!(mean_pairwise_jaccard(&normalized), 1.0);
    }

    #[test]
    fn mean_pairwise_jaccard_identical_inputs_yield_one() {
        let normalized = vec![
            (1, "line1\nline2".to_string(), vec![]),
            (2, "line1\nline2".to_string(), vec![]),
        ];
        assert!((mean_pairwise_jaccard(&normalized) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn mean_pairwise_jaccard_disjoint_inputs_yield_zero() {
        let normalized = vec![
            (1, "a\nb".to_string(), vec![]),
            (2, "c\nd".to_string(), vec![]),
        ];
        assert!(mean_pairwise_jaccard(&normalized) < 1e-9);
    }

    #[test]
    fn collect_differing_identifiers_only_reports_varied_positions() {
        let normalized = vec![
            (1, "n".to_string(), vec!["Foo".into(), "bar".into()]),
            (2, "n".to_string(), vec!["Bar".into(), "bar".into()]),
            (3, "n".to_string(), vec!["Baz".into(), "bar".into()]),
        ];
        let diffs = collect_differing_identifiers(&normalized);
        // Position 0 varies (Foo/Bar/Baz); position 1 is constant ("bar").
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0]["position"], 0);
        let values = diffs[0]["values"].as_array().unwrap();
        assert_eq!(values.len(), 3);
    }

    #[test]
    fn collect_differing_identifiers_empty_input_yields_empty() {
        assert!(collect_differing_identifiers(&[]).is_empty());
    }

    #[test]
    fn abstraction_for_language_dispatches() {
        assert!(abstraction_for_language("rust").contains("macro_rules"));
        assert!(abstraction_for_language("typescript").contains("generic"));
        assert!(abstraction_for_language("python").contains("TypeVar"));
        assert!(abstraction_for_language("unknown_lang").contains("parameterized"));
    }
}
