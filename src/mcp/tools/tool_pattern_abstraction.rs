//! `tool_pattern_abstraction` — trait / interface extraction candidates.
//!
//! Pulls chunk-pairs at *medium* similarity (default 0.70-0.85, exclusive
//! upper bound) where both endpoints belong to the same FCM topic with
//! membership above a threshold. Pairs sharing a topic but at medium
//! similarity are different implementations of one abstraction — perfect
//! candidates for trait / interface extraction.
//!
//! Distinct from `chunk_clusters` (near-duplicates) and
//! `extraction_candidates` (whole-file shared-crate moves).

#![allow(unused_imports)]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::json;
use tracing::debug;

use crate::context::SystemContext;
use crate::db::queries;
use crate::mcp::server::*;
use crate::mcp::tools::fix_actions::{
    EstimatedEffort, FixAction, PathRange, RecommendedFix, TargetPath,
};
use crate::mcp::tools::fix_helpers::{pool_or_err, propose_function_name};

pub async fn tool_pattern_abstraction(
    ctx: &SystemContext,
    params: PatternAbstractionParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .pattern_abstraction_scans
        .fetch_add(1, Ordering::Relaxed);

    let min_sim = params.min_similarity.unwrap_or(0.70).clamp(0.0, 1.0);
    let max_sim = params.max_similarity.unwrap_or(0.85).clamp(0.0, 1.0);
    let min_membership = params.min_topic_membership.unwrap_or(0.55).clamp(0.0, 1.0);
    let min_cluster_size = params.min_cluster_size.unwrap_or(4).max(2);
    let limit = params.limit.unwrap_or(20).max(1);
    let include_same_repo = params.include_same_repo.unwrap_or(false);
    let worktree_filter = params.worktree_filter.as_deref().unwrap_or("main");
    let main_only = matches!(worktree_filter, "main");

    if min_sim >= max_sim {
        return Err(McpError::invalid_params(
            format!(
                "min_similarity ({}) must be less than max_similarity ({})",
                min_sim, max_sim
            ),
            None,
        ));
    }

    debug!(
        tool = "pattern_abstraction_candidates",
        min_sim, max_sim, min_membership, min_cluster_size, worktree_filter, "MCP tool invoked",
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

    let raw_pair_cap = (limit as i64).saturating_mul(20).clamp(50, 5_000) as i32;
    let pairs = queries::find_pattern_abstraction_pairs(
        pool,
        min_sim,
        max_sim,
        min_membership,
        params.language.as_deref(),
        &main_ids,
        params.project.as_deref(),
        include_same_repo,
        raw_pair_cap,
    )
    .await
    .map_err(|e| McpError::internal_error(format!("Pattern-pair query failed: {}", e), None))?;

    if pairs.is_empty() {
        return Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&json!({
                "abstraction_candidates": [],
                "total_candidates": 0,
                "parameters": parameters_echo(&params, min_sim, max_sim, min_membership, min_cluster_size, limit, worktree_filter, include_same_repo),
                "guidance": "No medium-similarity pairs sharing a topic. Either no FCM topics have \
                             been computed (run `discover_topics`), or the similarity-scan cron \
                             hasn't run, or the threshold band is too narrow.",
                "health": health_envelope(false, false),
            }))
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?,
        )]));
    }

    // Group pairs by topic; cluster each group via union-find.
    let mut by_topic: HashMap<i64, Vec<&queries::PatternAbstractionPair>> = HashMap::new();
    for p in &pairs {
        by_topic.entry(p.topic_id).or_default().push(p);
    }

    let mut candidates: Vec<serde_json::Value> = Vec::new();
    for (topic_id, topic_pairs) in by_topic {
        let groups = cluster_pattern_pairs(topic_pairs.as_slice(), min_cluster_size);
        for grp in groups {
            // First-encountered topic metadata for this group.
            let topic_label = topic_pairs
                .iter()
                .find(|p| grp.chunk_ids.contains(&p.chunk_id_a))
                .map(|p| p.topic_label.clone())
                .unwrap_or_default();
            let topic_keywords: Vec<String> = topic_pairs
                .iter()
                .find(|p| grp.chunk_ids.contains(&p.chunk_id_a))
                .and_then(|p| p.topic_keywords.clone())
                .unwrap_or_default();

            // Average pair similarity over observed pairs in this group.
            let mut sim_sum = 0.0_f64;
            let mut sim_count = 0_u64;
            for p in topic_pairs.iter() {
                if grp.chunk_ids.contains(&p.chunk_id_a) && grp.chunk_ids.contains(&p.chunk_id_b) {
                    sim_sum += p.similarity;
                    sim_count += 1;
                }
            }
            let avg_pairwise = if sim_count > 0 {
                sim_sum / sim_count as f64
            } else {
                0.0
            };

            let project_count = grp.project_ids.len();
            let chunk_count = grp.chunk_ids.len();

            // Implementations list — one row per member chunk.
            let mut impls: Vec<serde_json::Value> = Vec::new();
            for &cid in &grp.chunk_ids {
                if let Some(meta) = grp.metadata.get(&cid) {
                    impls.push(json!({
                        "chunk_id": cid,
                        "file": meta.path,
                        "project": meta.project,
                        "language": meta.language,
                        "membership_score": format!("{:.4}", meta.membership),
                    }));
                }
            }

            // Build the abstraction proposal.
            let language = grp
                .metadata
                .values()
                .next()
                .map(|m| m.language.clone())
                .unwrap_or_else(|| "rust".to_string());
            let proposed_method = propose_function_name(&topic_keywords);
            let (abstraction_kind, abstraction_action) = abstraction_for_language(&language);
            let abstraction_name = abstraction_name_from_keywords(&topic_keywords);

            // Diversity-rewarded priority — fewer-similar impls of one topic is more valuable.
            let priority_score =
                (chunk_count as f64) * (project_count as f64) * (1.0 - avg_pairwise).max(0.0);

            // recommended_fix
            let project_for_fix = grp
                .first_project
                .clone()
                .unwrap_or_else(|| "unknown".to_string());
            let mut fix = RecommendedFix::new(abstraction_action, project_for_fix)
                .with_confidence(0.55)
                .with_effort(if chunk_count >= 6 || project_count >= 4 {
                    EstimatedEffort::Large
                } else {
                    EstimatedEffort::Medium
                });
            for &cid in &grp.chunk_ids {
                if let Some(meta) = grp.metadata.get(&cid) {
                    fix = fix.add_location(PathRange {
                        path: meta.path.clone(),
                        start_line: 1,
                        end_line: 1,
                    });
                }
            }
            fix = fix
                .add_target(TargetPath {
                    suggested_name: Some(abstraction_name.clone()),
                    ..Default::default()
                })
                .add_step(format!(
                    "Define {} `{}` with method `{}(...)` capturing the shared shape of these {} \
                     implementations across {} projects (avg similarity {:.2}, topic '{}'). \
                     Convert each implementation to an `impl` of the {}.",
                    abstraction_kind,
                    abstraction_name,
                    proposed_method,
                    chunk_count,
                    project_count,
                    avg_pairwise,
                    topic_label,
                    abstraction_kind,
                ));

            let fix_json = serde_json::to_value(&fix).map_err(|e| {
                McpError::internal_error(format!("Fix serialization failed: {}", e), None)
            })?;

            candidates.push(json!({
                "candidate_id": format!("ab_t{}_c{}", topic_id, grp.chunk_ids.first().copied().unwrap_or(0)),
                "shared_topic": {
                    "id": topic_id,
                    "label": topic_label,
                    "keywords": topic_keywords,
                },
                "chunk_count": chunk_count,
                "project_count": project_count,
                "avg_pairwise_similarity": format!("{:.4}", avg_pairwise),
                "language": language,
                "implementations": impls,
                "proposed_abstraction": {
                    "kind": abstraction_kind,
                    "name": abstraction_name,
                    "method": proposed_method,
                },
                "recommended_fix": fix_json,
                "priority_score": format!("{:.2}", priority_score),
            }));
        }
    }

    // Sort by priority descending, then chunk_count.
    candidates.sort_by(|a, b| {
        let pa: f64 = a["priority_score"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        let pb: f64 = b["priority_score"]
            .as_str()
            .and_then(|s| s.parse().ok())
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
    candidates.truncate(limit as usize);

    let total = candidates.len();
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
        "abstraction_candidates": candidates,
        "total_candidates": total,
        "parameters": parameters_echo(&params, min_sim, max_sim, min_membership, min_cluster_size, limit, worktree_filter, include_same_repo),
        "guidance": format!(
            "Top {} pattern-abstraction candidates ranked by chunk_count × project_count × (1 - avg_sim). \
             Lower similarity within a shared topic = more diverse implementations = stronger \
             abstraction signal. Each candidate carries a typed `recommended_fix(action=extract_trait \
             | extract_interface)`.",
            total
        ),
        "health": health_envelope(true, true),
    });
    let json_str = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "pattern_abstraction_candidates",
        candidates = total,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json_str)]))
}

// ============================================================================
// Cluster representation
// ============================================================================

#[derive(Debug, Clone)]
struct ChunkMeta {
    path: String,
    project: String,
    language: String,
    membership: f64,
}

#[derive(Debug)]
struct AbstractionGroup {
    chunk_ids: Vec<i64>,
    project_ids: HashSet<i32>,
    metadata: HashMap<i64, ChunkMeta>,
    first_project: Option<String>,
}

/// Within a single topic's pair list, union-find on chunk_id pairs.
/// Returns groups of size >= min_cluster_size.
fn cluster_pattern_pairs(
    pairs: &[&queries::PatternAbstractionPair],
    min_cluster_size: usize,
) -> Vec<AbstractionGroup> {
    if pairs.is_empty() {
        return Vec::new();
    }

    let mut chunk_ids: Vec<i64> = Vec::new();
    let mut id_to_idx: HashMap<i64, usize> = HashMap::new();
    let mut metadata: HashMap<i64, ChunkMeta> = HashMap::new();
    let mut project_id_per_chunk: HashMap<i64, i32> = HashMap::new();
    let mut first_project: Option<String> = None;

    for pair in pairs {
        for (cid, path, project, lang, member, project_id) in [
            (
                pair.chunk_id_a,
                &pair.path_a,
                &pair.project_name_a,
                &pair.language,
                pair.membership_a,
                pair.project_id_a,
            ),
            (
                pair.chunk_id_b,
                &pair.path_b,
                &pair.project_name_b,
                &pair.language,
                pair.membership_b,
                pair.project_id_b,
            ),
        ] {
            if let std::collections::hash_map::Entry::Vacant(e) = id_to_idx.entry(cid) {
                e.insert(chunk_ids.len());
                chunk_ids.push(cid);
            }
            metadata.entry(cid).or_insert_with(|| ChunkMeta {
                path: path.clone(),
                project: project.clone(),
                language: lang.clone(),
                membership: member,
            });
            project_id_per_chunk.entry(cid).or_insert(project_id);
            if first_project.is_none() {
                first_project = Some(project.clone());
            }
        }
    }

    let mut uf = UnionFind::new(chunk_ids.len());
    for pair in pairs {
        let ia = id_to_idx[&pair.chunk_id_a];
        let ib = id_to_idx[&pair.chunk_id_b];
        uf.union(ia, ib);
    }

    let mut groups_idx: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..chunk_ids.len() {
        groups_idx.entry(uf.find(i)).or_default().push(i);
    }

    let mut out: Vec<AbstractionGroup> = Vec::new();
    for (_, members) in groups_idx {
        if members.len() < min_cluster_size {
            continue;
        }
        let mut member_ids: Vec<i64> = members.iter().map(|&i| chunk_ids[i]).collect();
        member_ids.sort_unstable();
        let mut project_ids: HashSet<i32> = HashSet::new();
        for &cid in &member_ids {
            if let Some(pid) = project_id_per_chunk.get(&cid).copied() {
                project_ids.insert(pid);
            }
        }
        let meta_subset: HashMap<i64, ChunkMeta> = member_ids
            .iter()
            .filter_map(|cid| metadata.get(cid).cloned().map(|m| (*cid, m)))
            .collect();
        out.push(AbstractionGroup {
            chunk_ids: member_ids,
            project_ids,
            metadata: meta_subset,
            first_project: first_project.clone(),
        });
    }

    out.sort_by(|a, b| {
        b.chunk_ids
            .len()
            .cmp(&a.chunk_ids.len())
            .then_with(|| b.project_ids.len().cmp(&a.project_ids.len()))
    });
    out
}

/// Pick the abstraction kind + corresponding FixAction based on language.
fn abstraction_for_language(language: &str) -> (&'static str, FixAction) {
    match language {
        "rust" => ("trait", FixAction::ExtractTrait),
        "java" | "kotlin" | "csharp" | "typescript" | "javascript" => {
            ("interface", FixAction::ExtractInterface)
        }
        "python" => ("Protocol", FixAction::ExtractTrait),
        "clojure" | "clojurescript" => ("defprotocol", FixAction::ExtractTrait),
        _ => ("interface", FixAction::ExtractInterface),
    }
}

/// Convert keywords into a PascalCase abstraction name (e.g.
/// `["validate", "email"]` → `EmailValidator`).
fn abstraction_name_from_keywords(keywords: &[String]) -> String {
    let cleaned: Vec<String> = keywords
        .iter()
        .filter_map(|k| {
            let s: String = k.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
            if s.is_empty() { None } else { Some(s) }
        })
        .take(3)
        .collect();
    if cleaned.is_empty() {
        return "SharedAbstraction".to_string();
    }
    let mut pascal = String::new();
    for word in cleaned.iter().rev() {
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            for c in first.to_uppercase() {
                pascal.push(c);
            }
            for c in chars {
                pascal.push(c.to_ascii_lowercase());
            }
        }
    }
    pascal
}

#[allow(clippy::too_many_arguments)]
fn parameters_echo(
    params: &PatternAbstractionParams,
    min_sim: f64,
    max_sim: f64,
    min_membership: f64,
    min_cluster_size: usize,
    limit: i32,
    worktree_filter: &str,
    include_same_repo: bool,
) -> serde_json::Value {
    json!({
        "min_similarity": min_sim,
        "max_similarity": max_sim,
        "min_topic_membership": min_membership,
        "min_cluster_size": min_cluster_size,
        "language": params.language,
        "project": params.project,
        "limit": limit,
        "worktree_filter": worktree_filter,
        "include_same_repo": include_same_repo,
    })
}

fn health_envelope(similarity_present: bool, topics_present: bool) -> serde_json::Value {
    json!({
        "similarity_stale": !similarity_present,
        "topics_present": topics_present,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abstraction_for_language_dispatches() {
        assert_eq!(abstraction_for_language("rust").0, "trait");
        assert_eq!(abstraction_for_language("java").0, "interface");
        assert_eq!(abstraction_for_language("python").0, "Protocol");
        assert_eq!(abstraction_for_language("typescript").0, "interface");
        assert_eq!(abstraction_for_language("clojure").0, "defprotocol");
        assert_eq!(abstraction_for_language("clojurescript").0, "defprotocol");
        assert_eq!(abstraction_for_language("c").0, "interface");
    }

    #[test]
    fn abstraction_name_from_keywords_is_pascal_reversed() {
        // Keywords ordered from most-relevant to least → reverse for natural noun-first naming.
        let kws = vec!["validate".to_string(), "email".to_string()];
        // ["validate","email"] reversed → ["email","validate"] → "EmailValidate"
        assert_eq!(abstraction_name_from_keywords(&kws), "EmailValidate");
    }

    #[test]
    fn abstraction_name_from_keywords_strips_punctuation() {
        let kws = vec!["build-request!".to_string(), "headers".to_string()];
        // → ["headers", "buildrequest"] reversed → "HeadersBuildrequest"... wait
        // The function takes top-3 keywords in order, then reverses for pascal.
        // ["build-request!" → "buildrequest", "headers"] → take(3) → ["buildrequest", "headers"]
        // → reversed iter → ["headers", "buildrequest"] → "Headers" + "Buildrequest" = "HeadersBuildrequest"
        assert_eq!(abstraction_name_from_keywords(&kws), "HeadersBuildrequest");
    }

    #[test]
    fn abstraction_name_from_keywords_fallback_for_empty() {
        assert_eq!(abstraction_name_from_keywords(&[]), "SharedAbstraction");
        assert_eq!(
            abstraction_name_from_keywords(&["!@#".to_string()]),
            "SharedAbstraction"
        );
    }
}
