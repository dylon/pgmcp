//! `tool_reviewer_recommender` — rank reviewers by recent file ownership.
//!
//! Per-file: pull the dominant `blame_author` within `recency_window_days`.
//! Aggregate to per-author file lists. Build a greedy minimum cover-set
//! (≥80% of files with the fewest reviewers).
//!
//! Files without blame coverage land in `unowned_files` with a note.

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
use crate::mcp::tools::fix_helpers::pool_or_err;

pub async fn tool_reviewer_recommender(
    ctx: &SystemContext,
    params: ReviewerRecommenderParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .reviewer_recommendations
        .fetch_add(1, Ordering::Relaxed);

    if params.files.is_empty() {
        return Err(McpError::invalid_params(
            "reviewer_recommender requires at least one file in `files`".to_string(),
            None,
        ));
    }
    let recency_window_days = params.recency_window_days.unwrap_or(365).max(1);
    let exclude: HashSet<String> = params
        .exclude_authors
        .clone()
        .unwrap_or_default()
        .into_iter()
        .collect();

    debug!(
        tool = "reviewer_recommender",
        project = %params.project,
        file_count = params.files.len(),
        recency_window_days,
        excluded = exclude.len(),
        "MCP tool invoked",
    );

    let pool = pool_or_err(ctx)?;
    let rows = queries::find_dominant_authors_for_files(
        pool,
        &params.project,
        &params.files,
        recency_window_days,
    )
    .await
    .map_err(|e| McpError::internal_error(format!("Author query failed: {}", e), None))?;

    // Build per-file author + per-author file list.
    let row_by_path: HashMap<String, queries::FileAuthorRow> = rows
        .into_iter()
        .map(|r| (r.relative_path.clone(), r))
        .collect();

    let mut per_author: HashMap<String, Vec<String>> = HashMap::new();
    let mut per_author_last_touch: HashMap<String, i32> = HashMap::new();
    let mut unowned: Vec<String> = Vec::new();

    for path in &params.files {
        match row_by_path.get(path).and_then(|r| r.top_author.clone()) {
            Some(author) if !exclude.contains(&author) => {
                per_author
                    .entry(author.clone())
                    .or_default()
                    .push(path.clone());
                if let Some(last) = row_by_path.get(path).and_then(|r| r.last_touch_days) {
                    let entry = per_author_last_touch.entry(author).or_insert(i32::MAX);
                    if last < *entry {
                        *entry = last;
                    }
                }
            }
            _ => unowned.push(path.clone()),
        }
    }

    // Sort reviewers by descending coverage, then by ascending last-touch (recently active first).
    let total_files = params.files.len();
    let mut reviewers: Vec<(String, Vec<String>, i32)> = per_author
        .into_iter()
        .map(|(author, files)| {
            let last_touch = per_author_last_touch
                .get(&author)
                .copied()
                .unwrap_or(i32::MAX);
            (author, files, last_touch)
        })
        .collect();
    reviewers.sort_by(|a, b| {
        b.1.len()
            .cmp(&a.1.len())
            .then_with(|| a.2.cmp(&b.2))
            .then_with(|| a.0.cmp(&b.0))
    });

    // Greedy min-cover: pick reviewers covering the most uncovered files
    // until we hit 80% coverage.
    let target = ((total_files as f64) * 0.8).ceil() as usize;
    let mut covered: HashSet<String> = HashSet::new();
    let mut minimum_set: Vec<String> = Vec::new();
    while covered.len() < target {
        let best = reviewers
            .iter()
            .filter(|(author, _, _)| !minimum_set.contains(author))
            .map(|(author, files, _)| {
                let new = files.iter().filter(|f| !covered.contains(*f)).count();
                (author.clone(), new)
            })
            .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)));
        match best {
            Some((author, count)) if count > 0 => {
                if let Some((_, files, _)) = reviewers.iter().find(|(a, _, _)| *a == author) {
                    for f in files {
                        covered.insert(f.clone());
                    }
                }
                minimum_set.push(author);
            }
            _ => break,
        }
    }

    // Build the per-reviewer JSON list.
    let reviewers_json: Vec<serde_json::Value> = reviewers
        .iter()
        .map(|(author, files, last_touch)| {
            let coverage_pct = files.len() as f64 / total_files.max(1) as f64;
            let last_touch_value = if *last_touch == i32::MAX {
                serde_json::Value::Null
            } else {
                serde_json::Value::from(*last_touch)
            };
            json!({
                "author": author,
                "files_owned": files.len(),
                "coverage_pct": format!("{:.4}", coverage_pct),
                "last_touch_days": last_touch_value,
                "file_breakdown": files,
            })
        })
        .collect();

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
        "project": params.project,
        "file_count": total_files,
        "reviewers": reviewers_json,
        "minimum_set": minimum_set,
        "unowned_files": unowned,
        "guidance": format!(
            "{} candidate reviewers covering {}/{} files. Minimum-cover set targets ≥80% \
             coverage with the fewest reviewers. Files in `unowned_files` lack recent blame \
             data within the {}-day window.",
            reviewers.len(),
            total_files - unowned.len(),
            total_files,
            recency_window_days
        ),
        "health": health_envelope(!row_by_path.is_empty()),
    });
    let json_str = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "reviewer_recommender",
        reviewers = reviewers.len(),
        unowned = unowned.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json_str)]))
}

fn health_envelope(blame_present: bool) -> serde_json::Value {
    json!({
        "blame_present": blame_present,
    })
}
