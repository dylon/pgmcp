//! `tool_recommend_module_split` — propose explicit splits for god files.
//!
//! Strategy: for each file with `line_count >= min_lines`, group its chunks
//! by their dominant FCM topic. Each topic-group of size >= 2 becomes a
//! suggested sub-file with line ranges. Files whose chunks all collapse to
//! one topic are cohesive and get an `add_test` recommendation instead.
//!
//! This is the topic-based variant. A future Tier-0e (tree-sitter) version
//! will use real chunk-internal call graphs (Louvain on symbol-reference
//! edges) for languages where symbol data is available — confidence rises
//! from 0.55 to ~0.75 when symbols are present.

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
use crate::mcp::tools::fix_helpers::{infer_module_name_from_topics, pool_or_err};

pub async fn tool_recommend_module_split(
    ctx: &SystemContext,
    params: RecommendModuleSplitParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .split_recommendations
        .fetch_add(1, Ordering::Relaxed);

    let min_lines = params.min_lines.unwrap_or(500).max(50);
    let limit = params.limit.unwrap_or(10).max(1);
    let min_communities = params.min_communities.unwrap_or(2).max(2);
    let include_chunks = params.include_chunks.unwrap_or(false);

    debug!(
        tool = "recommend_module_split",
        project = %params.project,
        min_lines,
        limit,
        min_communities,
        "MCP tool invoked",
    );

    let pool = pool_or_err(ctx)?;
    let rows = queries::get_god_file_chunks_with_topics(pool, &params.project, min_lines)
        .await
        .map_err(|e| {
            McpError::internal_error(format!("God-file chunks query failed: {}", e), None)
        })?;

    if rows.is_empty() {
        return Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&json!({
                "splits": [],
                "total_splits": 0,
                "parameters": parameters_echo(&params, min_lines, limit, min_communities, include_chunks),
                "guidance": format!(
                    "No files >= {} lines found in project '{}'.",
                    min_lines, params.project
                ),
                "health": health_envelope(false, false),
            }))
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?,
        )]));
    }

    // Bucket rows by file_id.
    let mut by_file: HashMap<i64, Vec<queries::GodFileChunkRow>> = HashMap::new();
    for r in rows {
        by_file.entry(r.file_id).or_default().push(r);
    }

    // Track topic-presence: when not a single chunk has a topic, soft-fail.
    let mut topics_present_anywhere = false;
    for chunks in by_file.values() {
        if chunks.iter().any(|c| c.topic_id.is_some()) {
            topics_present_anywhere = true;
            break;
        }
    }
    if !topics_present_anywhere {
        return Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&json!({
                "splits": [],
                "total_splits": 0,
                "parameters": parameters_echo(&params, min_lines, limit, min_communities, include_chunks),
                "guidance": "Topics absent for all god files. Run `discover_topics` first; \
                             this tool groups chunks by their dominant FCM topic.",
                "health": health_envelope(true, false),
            }))
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?,
        )]));
    }

    let mut splits: Vec<serde_json::Value> = Vec::new();
    for (file_id, mut chunks) in by_file {
        chunks.sort_by_key(|c| c.chunk_index);
        if chunks.len() < min_communities {
            continue;
        }

        // Group chunks by topic_id. Chunks without a topic land in the
        // sentinel bucket -1.
        let mut by_topic: HashMap<i64, Vec<&queries::GodFileChunkRow>> = HashMap::new();
        for c in &chunks {
            let key = c.topic_id.unwrap_or(-1);
            by_topic.entry(key).or_default().push(c);
        }
        // Drop singleton topics from consideration as split points
        // (a chunk alone isn't a sub-file unless we have many of them).
        let valid_groups: Vec<(i64, Vec<&queries::GodFileChunkRow>)> =
            by_topic.into_iter().filter(|(_, v)| v.len() >= 2).collect();

        let file_meta = chunks
            .first()
            .expect("chunks non-empty by construction")
            .clone();

        if valid_groups.len() < min_communities {
            // Cohesive god file: recommend tests instead of split.
            let fix = RecommendedFix::new(FixAction::AddTest, params.project.clone())
                .with_confidence(0.55)
                .with_effort(EstimatedEffort::Medium)
                .add_location(PathRange {
                    path: file_meta.relative_path.clone(),
                    start_line: 1,
                    end_line: file_meta.line_count.max(1) as u32,
                })
                .add_step(format!(
                    "{} is large ({} lines, {} chunks) but cohesive — chunks collapse to fewer \
                     than {} distinct topics. Splitting would fragment the abstraction. Add \
                     integration tests that pin current behavior so future refactors are safe.",
                    file_meta.relative_path,
                    file_meta.line_count,
                    chunks.len(),
                    min_communities
                ));
            let fix_json = serde_json::to_value(&fix).map_err(|e| {
                McpError::internal_error(format!("Fix serialization failed: {}", e), None)
            })?;
            splits.push(json!({
                "file": file_meta.relative_path,
                "file_id": file_id,
                "line_count": file_meta.line_count,
                "chunk_count": chunks.len(),
                "topic_group_count": valid_groups.len(),
                "severity": "low",
                "verdict": "cohesive",
                "why_it_matters": format!(
                    "Large but cohesive: not a split candidate. Add tests to lock in behavior."
                ),
                "recommended_fix": fix_json,
            }));
            continue;
        }

        // Build per-topic-group split targets.
        let mut groups_meta: Vec<TopicGroupMeta> = Vec::new();
        for (topic_id, group) in &valid_groups {
            let label = group
                .iter()
                .find_map(|c| c.topic_label.clone())
                .unwrap_or_else(|| {
                    if *topic_id == -1 {
                        "untyped".to_string()
                    } else {
                        format!("topic_{}", topic_id)
                    }
                });
            let keywords: Vec<String> = group
                .iter()
                .find_map(|c| c.topic_keywords.clone())
                .unwrap_or_default();
            let mut line_ranges: Vec<(i32, i32)> =
                group.iter().map(|c| (c.start_line, c.end_line)).collect();
            line_ranges.sort_by_key(|(s, _)| *s);
            let merged_ranges = merge_contiguous_ranges(&line_ranges);
            groups_meta.push(TopicGroupMeta {
                topic_id: *topic_id,
                label,
                keywords,
                line_ranges: merged_ranges,
                chunk_count: group.len(),
            });
        }
        groups_meta.sort_by_key(|g| g.line_ranges.first().map(|r| r.0).unwrap_or(0));

        // Determine destination directory: same dir as the original file.
        let dir = file_meta
            .relative_path
            .rsplit_once('/')
            .map(|(d, _)| d.to_string())
            .unwrap_or_default();

        // Build the recommended_fix.
        let mut fix = RecommendedFix::new(FixAction::SplitFile, params.project.clone())
            .with_confidence(0.55)
            .with_effort(if chunks.len() >= 12 || valid_groups.len() >= 4 {
                EstimatedEffort::Large
            } else {
                EstimatedEffort::Medium
            })
            .add_location(PathRange {
                path: file_meta.relative_path.clone(),
                start_line: 1,
                end_line: file_meta.line_count.max(1) as u32,
            });
        for (idx, grp) in groups_meta.iter().enumerate() {
            let suggested_name = build_suggested_filename(
                &dir,
                &file_meta.relative_path,
                &grp.keywords,
                &grp.label,
                idx,
                &file_meta.language,
            );
            fix = fix.add_target(TargetPath {
                suggested_new_path: Some(suggested_name.clone()),
                line_ranges: Some(
                    grp.line_ranges
                        .iter()
                        .map(|&(s, e)| (s.max(1) as u32, e.max(1) as u32))
                        .collect(),
                ),
                ..Default::default()
            });
            fix = fix.add_step(format!(
                "Create {} from line ranges {:?} of {} (topic '{}', {} chunks).",
                suggested_name,
                grp.line_ranges,
                file_meta.relative_path,
                grp.label,
                grp.chunk_count
            ));
        }
        fix = fix.add_step(format!(
            "Update imports referencing the original file. Run `change_impact_analysis` on \
             {} to enumerate downstream files that need updating.",
            file_meta.relative_path
        ));
        let fix_json = serde_json::to_value(&fix).map_err(|e| {
            McpError::internal_error(format!("Fix serialization failed: {}", e), None)
        })?;

        let mut output_row = json!({
            "file": file_meta.relative_path,
            "file_id": file_id,
            "line_count": file_meta.line_count,
            "chunk_count": chunks.len(),
            "topic_group_count": valid_groups.len(),
            "severity": if chunks.len() >= 12 { "high" } else { "medium" },
            "verdict": "splittable",
            "why_it_matters": format!(
                "{} lines spread across {} distinct topics — splitting along topic boundaries \
                 reduces cognitive load and import blast-radius.",
                file_meta.line_count,
                valid_groups.len()
            ),
            "topic_groups": groups_meta.iter().map(|g| json!({
                "topic_id": g.topic_id,
                "label": g.label,
                "keywords": g.keywords,
                "chunk_count": g.chunk_count,
                "line_ranges": g.line_ranges.iter().map(|(s, e)| format!("{}-{}", s, e)).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "recommended_fix": fix_json,
        });
        if include_chunks {
            output_row["chunks"] = json!(
                chunks
                    .iter()
                    .map(|c| json!({
                        "chunk_id": c.chunk_id,
                        "chunk_index": c.chunk_index,
                        "lines": format!("{}-{}", c.start_line, c.end_line),
                        "topic_id": c.topic_id,
                        "topic_label": c.topic_label,
                        "membership_score": c.membership_score,
                    }))
                    .collect::<Vec<_>>()
            );
        }
        splits.push(output_row);
    }

    // Sort: splittable verdicts first, then by chunk_count descending.
    splits.sort_by(|a, b| {
        let va = a["verdict"].as_str().unwrap_or("") == "splittable";
        let vb = b["verdict"].as_str().unwrap_or("") == "splittable";
        match (va, vb) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => b["chunk_count"]
                .as_u64()
                .unwrap_or(0)
                .cmp(&a["chunk_count"].as_u64().unwrap_or(0)),
        }
    });
    splits.truncate(limit as usize);
    let total = splits.len();

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
        "splits": splits,
        "total_splits": total,
        "parameters": parameters_echo(&params, min_lines, limit, min_communities, include_chunks),
        "guidance": "Each `splittable` row carries a typed `recommended_fix(action=split_file)` \
                     with line-range targets. `cohesive` rows propose `add_test` instead — \
                     splitting cohesive code fragments the abstraction.",
        "health": health_envelope(true, true),
    });
    let json_str = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "recommend_module_split",
        splits = total,
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json_str)]))
}

// ============================================================================
// Helpers
// ============================================================================

#[derive(Debug, Clone)]
struct TopicGroupMeta {
    topic_id: i64,
    label: String,
    keywords: Vec<String>,
    line_ranges: Vec<(i32, i32)>,
    chunk_count: usize,
}

/// Merge contiguous (or near-contiguous) line ranges. Two ranges are merged
/// when their gap is <=1 — this collapses chunk boundaries that often sit at
/// adjacent lines.
fn merge_contiguous_ranges(ranges: &[(i32, i32)]) -> Vec<(i32, i32)> {
    if ranges.is_empty() {
        return Vec::new();
    }
    let mut sorted: Vec<(i32, i32)> = ranges.to_vec();
    sorted.sort_by_key(|(s, _)| *s);
    let mut out: Vec<(i32, i32)> = Vec::new();
    for (s, e) in sorted {
        if let Some(last) = out.last_mut()
            && s <= last.1 + 1
        {
            last.1 = last.1.max(e);
        } else {
            out.push((s, e));
        }
    }
    out
}

/// Compose a suggested filename for a sub-file.
/// Tries: `<dir>/<top-keyword>.<ext>`. Falls back to
/// `<dir>/<basename>_part_<idx>.<ext>` when no keywords are available.
fn build_suggested_filename(
    dir: &str,
    original_path: &str,
    keywords: &[String],
    _label: &str,
    fallback_idx: usize,
    language: &str,
) -> String {
    let basename = original_path.rsplit('/').next().unwrap_or(original_path);
    let stem = basename
        .rsplit_once('.')
        .map(|(s, _)| s)
        .unwrap_or(basename);
    let ext = match language {
        "rust" => "rs",
        "python" => "py",
        "typescript" => "ts",
        "javascript" => "js",
        "go" => "go",
        "java" => "java",
        "c" => "c",
        "cpp" => "cpp",
        _ => basename.rsplit_once('.').map(|(_, e)| e).unwrap_or("rs"),
    };

    let kw_stem: String = keywords
        .iter()
        .filter_map(|k| {
            let cleaned: String = k
                .to_ascii_lowercase()
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if cleaned.is_empty() {
                None
            } else {
                Some(cleaned)
            }
        })
        .next()
        .unwrap_or_else(|| format!("{}_part_{}", stem, fallback_idx));

    let prefix = if dir.is_empty() {
        String::new()
    } else {
        format!("{}/", dir)
    };
    format!("{}{}.{}", prefix, kw_stem, ext)
}

fn parameters_echo(
    params: &RecommendModuleSplitParams,
    min_lines: i32,
    limit: i32,
    min_communities: usize,
    include_chunks: bool,
) -> serde_json::Value {
    json!({
        "project": params.project,
        "min_lines": min_lines,
        "limit": limit,
        "min_communities": min_communities,
        "include_chunks": include_chunks,
    })
}

fn health_envelope(god_files_present: bool, topics_present: bool) -> serde_json::Value {
    json!({
        "god_files_present": god_files_present,
        "topics_present": topics_present,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_contiguous_ranges_combines_adjacent() {
        // Adjacent ranges (gap of 1) merge.
        let merged = merge_contiguous_ranges(&[(1, 100), (101, 200), (300, 400)]);
        assert_eq!(merged, vec![(1, 200), (300, 400)]);
    }

    #[test]
    fn merge_contiguous_ranges_sorts_input() {
        let merged = merge_contiguous_ranges(&[(300, 400), (1, 100), (101, 200)]);
        assert_eq!(merged, vec![(1, 200), (300, 400)]);
    }

    #[test]
    fn merge_contiguous_ranges_empty_input() {
        assert!(merge_contiguous_ranges(&[]).is_empty());
    }

    #[test]
    fn merge_contiguous_ranges_handles_overlap() {
        let merged = merge_contiguous_ranges(&[(1, 100), (50, 150)]);
        assert_eq!(merged, vec![(1, 150)]);
    }

    #[test]
    fn build_suggested_filename_uses_top_keyword() {
        let f = build_suggested_filename(
            "src/cli",
            "src/cli/mod.rs",
            &["dispatch".to_string(), "router".into()],
            "command-routing",
            0,
            "rust",
        );
        assert_eq!(f, "src/cli/dispatch.rs");
    }

    #[test]
    fn build_suggested_filename_fallback_when_no_keywords() {
        let f = build_suggested_filename("src/cli", "src/cli/mod.rs", &[], "untyped", 2, "rust");
        assert_eq!(f, "src/cli/mod_part_2.rs");
    }

    #[test]
    fn build_suggested_filename_uses_language_extension() {
        let f = build_suggested_filename(
            "src",
            "src/big.py",
            &["validation".into()],
            "validation",
            0,
            "python",
        );
        assert_eq!(f, "src/validation.py");
    }

    #[test]
    fn build_suggested_filename_no_dir() {
        let f = build_suggested_filename("", "huge.rs", &["parser".into()], "parser", 0, "rust");
        assert_eq!(f, "parser.rs");
    }
}
