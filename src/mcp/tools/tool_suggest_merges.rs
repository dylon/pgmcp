//! `tool_suggest_merges` — MCP tool body, extracted from `super::super::server`.

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

pub async fn tool_suggest_merges(
    ctx: &SystemContext,
    params: SuggestMergesParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().merge_scans.fetch_add(1, Ordering::Relaxed);

    let language_param = params.language.as_deref().unwrap_or("markdown");
    let language_filter = if language_param == "*" {
        None
    } else {
        Some(language_param)
    };
    let min_overlap = params.min_overlap.unwrap_or(0.4);
    let limit = params.limit.unwrap_or(20);

    debug!(
        tool = "suggest_merges",
        project = %params.project,
        language = language_param,
        min_overlap,
        limit,
        "MCP tool invoked",
    );

    let rows = ctx
        .db()
        .get_file_topic_distributions(&params.project, language_filter)
        .await
        .map_err(|e| McpError::internal_error(format!("Query failed: {}", e), None))?;

    if rows.is_empty() {
        return Ok(CallToolResult::success(vec![Content::text(
            "No topic assignments found for the specified project/language. \
             Run discover_topics first.",
        )]));
    }

    // Build per-file topic distributions: file_id -> Vec<(topic_id, total_membership)>
    use std::collections::HashMap;

    struct FileMeta {
        path: String,
        relative_path: String,
        line_count: i32,
        size_bytes: i64,
        topics: HashMap<i32, (f64, String)>, // topic_id -> (total_membership, label)
    }

    let mut files: HashMap<i64, FileMeta> = HashMap::new();
    for row in &rows {
        let entry = files.entry(row.file_id).or_insert_with(|| FileMeta {
            path: row.path.clone(),
            relative_path: row.relative_path.clone(),
            line_count: row.line_count,
            size_bytes: row.size_bytes,
            topics: HashMap::new(),
        });
        entry.topics.insert(
            row.topic_id,
            (row.total_membership, row.topic_label.clone()),
        );
    }

    let file_ids: Vec<i64> = files.keys().copied().collect();
    let n = file_ids.len();

    if n < 2 {
        return Ok(CallToolResult::success(vec![Content::text(
            "Need at least 2 files with topic assignments for merge analysis.",
        )]));
    }

    // Compute pairwise weighted Jaccard and collect qualifying pairs
    struct MergePair {
        file_a: i64,
        file_b: i64,
        overlap: f64,
        shared_topics: Vec<String>,
    }

    let mut qualifying_pairs: Vec<MergePair> = Vec::new();

    for i in 0..n {
        for j in (i + 1)..n {
            let fa = &files[&file_ids[i]];
            let fb = &files[&file_ids[j]];

            // Weighted Jaccard: sum(min weights) / sum(max weights) over all topics
            let mut intersection_sum = 0.0f64;
            let mut union_sum = 0.0f64;
            let mut shared = Vec::new();

            // All topic IDs from both files
            let mut all_topic_ids: std::collections::HashSet<i32> =
                fa.topics.keys().copied().collect();
            all_topic_ids.extend(fb.topics.keys());

            for &tid in &all_topic_ids {
                let wa = fa.topics.get(&tid).map(|(m, _)| *m).unwrap_or(0.0);
                let wb = fb.topics.get(&tid).map(|(m, _)| *m).unwrap_or(0.0);
                intersection_sum += wa.min(wb);
                union_sum += wa.max(wb);

                if wa > 0.0 && wb > 0.0 {
                    let label = fa
                        .topics
                        .get(&tid)
                        .or_else(|| fb.topics.get(&tid))
                        .map(|(_, l)| l.clone())
                        .unwrap_or_default();
                    shared.push(label);
                }
            }

            let overlap = if union_sum > 0.0 {
                intersection_sum / union_sum
            } else {
                0.0
            };

            if overlap >= min_overlap {
                qualifying_pairs.push(MergePair {
                    file_a: file_ids[i],
                    file_b: file_ids[j],
                    overlap,
                    shared_topics: shared,
                });
            }
        }
    }

    if qualifying_pairs.is_empty() {
        let result = serde_json::json!({
            "project": params.project,
            "language": language_param,
            "merge_groups_found": 0,
            "merge_groups": [],
            "guidance": "No file pairs found with topic overlap above the threshold. \
                         Try lowering min_overlap or broadening the language filter.",
        });
        let json = serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;
        return Ok(CallToolResult::success(vec![Content::text(json)]));
    }

    // Cluster with UnionFind
    let mut id_to_idx: HashMap<i64, usize> = HashMap::new();
    let mut idx_file_ids: Vec<i64> = Vec::new();
    for pair in &qualifying_pairs {
        if let std::collections::hash_map::Entry::Vacant(e) = id_to_idx.entry(pair.file_a) {
            e.insert(idx_file_ids.len());
            idx_file_ids.push(pair.file_a);
        }
        if let std::collections::hash_map::Entry::Vacant(e) = id_to_idx.entry(pair.file_b) {
            e.insert(idx_file_ids.len());
            idx_file_ids.push(pair.file_b);
        }
    }

    let mut uf = UnionFind::new(idx_file_ids.len());
    let mut pair_overlaps: HashMap<(usize, usize), (f64, Vec<String>)> = HashMap::new();

    for pair in &qualifying_pairs {
        let ia = id_to_idx[&pair.file_a];
        let ib = id_to_idx[&pair.file_b];
        uf.union(ia, ib);
        pair_overlaps.insert(
            (ia.min(ib), ia.max(ib)),
            (pair.overlap, pair.shared_topics.clone()),
        );
    }

    // Collect clusters
    let mut clusters: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..idx_file_ids.len() {
        let root = uf.find(i);
        clusters.entry(root).or_default().push(i);
    }

    // Format merge groups
    let mut merge_groups: Vec<serde_json::Value> = Vec::new();
    for members in clusters.values() {
        if members.len() < 2 {
            continue;
        }

        let mut group_files = Vec::new();
        let mut total_lines: i64 = 0;
        let mut all_shared_topics: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut overlap_sum = 0.0f64;
        let mut overlap_count = 0usize;

        for &idx in members {
            let fid = idx_file_ids[idx];
            let fm = &files[&fid];
            total_lines += fm.line_count as i64;

            let topic_labels: Vec<&str> = fm.topics.values().map(|(_, l)| l.as_str()).collect();

            group_files.push(serde_json::json!({
                "path": fm.path,
                "relative_path": fm.relative_path,
                "line_count": fm.line_count,
                "size_bytes": fm.size_bytes,
                "topic_count": fm.topics.len(),
                "topics": topic_labels,
            }));
        }

        for i in 0..members.len() {
            for j in (i + 1)..members.len() {
                let key = (members[i].min(members[j]), members[i].max(members[j]));
                if let Some((ov, shared)) = pair_overlaps.get(&key) {
                    overlap_sum += ov;
                    overlap_count += 1;
                    all_shared_topics.extend(shared.iter().cloned());
                }
            }
        }

        let avg_overlap = if overlap_count > 0 {
            overlap_sum / overlap_count as f64
        } else {
            0.0
        };

        let shared_vec: Vec<String> = all_shared_topics.into_iter().collect();

        merge_groups.push(serde_json::json!({
            "files": group_files,
            "shared_topics": shared_vec,
            "avg_overlap": format!("{:.4}", avg_overlap),
            "total_line_count": total_lines,
            "file_count": members.len(),
        }));
    }

    // Sort by avg_overlap descending
    merge_groups.sort_by(|a, b| {
        let sa: f64 = a["avg_overlap"]
            .as_str()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0.0);
        let sb: f64 = b["avg_overlap"]
            .as_str()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0.0);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });
    merge_groups.truncate(limit as usize);

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

    let result = serde_json::json!({
        "effect_breakdown": effect_breakdown,
        "project": params.project,
        "language": language_param,
        "min_overlap": min_overlap,
        "merge_groups_found": merge_groups.len(),
        "merge_groups": merge_groups,
        "guidance": "Files in the same merge group cover overlapping topics. \
                     Consider consolidating them to reduce documentation fragmentation. \
                     High avg_overlap indicates redundant topic coverage.",
    });

    let json = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "suggest_merges",
        groups = merge_groups.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
