//! `tool_pr_scope` — recommend min/recommended/max PR scope from a starter file.
//!
//! Three nested sets:
//! - `min`     = direct importers (1-hop reverse BFS over import edges)
//! - `recommended` = `min` ∪ co-change Jaccard ≥ co_change_min
//! - `max`     = `recommended` ∪ depth-N reverse BFS ∪ topic-neighbor files
//!
//! Verdict: `len(max) / len(recommended)` →
//!   < 2.0 = "focused", < 5.0 = "normal", else "sprawling".

#![allow(unused_imports)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::Ordering;
use std::time::Instant;

use petgraph::Direction;
use petgraph::graph::NodeIndex;
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::json;
use tracing::{debug, info};

use crate::context::SystemContext;
use crate::db::queries;
use crate::mcp::server::*;
use crate::mcp::tools::fix_helpers::{load_import_graph, lookup_project_id, pool_or_err};

pub async fn tool_pr_scope(
    ctx: &SystemContext,
    params: PrScopeRecommenderParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .pr_scope_recommendations
        .fetch_add(1, Ordering::Relaxed);

    let co_change_min = params.co_change_min.unwrap_or(0.4).clamp(0.0, 1.0);
    let impact_depth = params.impact_depth.unwrap_or(2).max(1) as usize;
    let include_topic_neighbors = params.include_topic_neighbors.unwrap_or(true);

    info!(
        tool = "pr_scope_recommender",
        project = %params.project,
        file = %params.file,
        co_change_min,
        impact_depth,
        include_topic_neighbors,
        "MCP tool invoked",
    );

    let pool = pool_or_err(ctx)?;
    let project_id = lookup_project_id(ctx, &params.project)
        .await?
        .ok_or_else(|| {
            McpError::invalid_params(format!("Project not found: {}", params.project), None)
        })?;
    let bundle = load_import_graph(ctx, project_id).await?;

    // Locate the seed file's NodeIndex.
    let seed = bundle
        .file_metas
        .iter()
        .find(|f| f.relative_path == params.file)
        .ok_or_else(|| {
            McpError::invalid_params(format!("File not found in project: {}", params.file), None)
        })?;
    let seed_idx = bundle
        .graph
        .file_id_to_node
        .get(&seed.file_id)
        .copied()
        .unwrap_or_else(|| panic!("seed file present in metadata but not in graph"));

    // 1) `min` — direct importers (1-hop reverse).
    let mut min_set: HashSet<String> = HashSet::new();
    let mut min_with_score: Vec<ScoredPath> = Vec::new();
    for n in bundle
        .graph
        .graph
        .neighbors_directed(seed_idx, Direction::Incoming)
    {
        if let Some(path) = bundle
            .graph
            .graph
            .node_weight(n)
            .map(|f| f.relative_path.clone())
            && path != params.file
        {
            min_set.insert(path.clone());
            min_with_score.push(ScoredPath {
                path,
                reason: "direct importer".to_string(),
                score: 1.0,
            });
        }
    }

    // 2) recommended — min ∪ co-change Jaccard ≥ threshold.
    let mut recommended_set: HashSet<String> = min_set.clone();
    let mut recommended_with_score: Vec<ScoredPath> = min_with_score.clone();
    let coupling_pairs = ctx
        .db()
        .find_coupled_files(&params.project, co_change_min, 2)
        .await
        .unwrap_or_default();
    let mut git_history_present = !coupling_pairs.is_empty();
    for pair in &coupling_pairs {
        let other = if pair.file_a == params.file {
            Some(pair.file_b.clone())
        } else if pair.file_b == params.file {
            Some(pair.file_a.clone())
        } else {
            None
        };
        if let Some(other_path) = other
            && other_path != params.file
            && recommended_set.insert(other_path.clone())
        {
            recommended_with_score.push(ScoredPath {
                path: other_path,
                reason: format!("co_change_jaccard={:.2}", pair.jaccard),
                score: pair.jaccard,
            });
        }
    }
    if !git_history_present && let Ok(present) = pool.execute_dummy(&params.project).await {
        git_history_present = present;
    }

    // 3) max — recommended ∪ depth-N reverse BFS ∪ topic neighbors.
    let mut max_set: HashSet<String> = recommended_set.clone();
    let mut max_with_score: Vec<ScoredPath> = recommended_with_score.clone();

    // Depth-N reverse BFS.
    let mut visited: HashSet<NodeIndex> = HashSet::new();
    let mut queue: VecDeque<(NodeIndex, usize)> = VecDeque::new();
    visited.insert(seed_idx);
    queue.push_back((seed_idx, 0));
    while let Some((node, depth)) = queue.pop_front() {
        if depth >= impact_depth {
            continue;
        }
        for n in bundle
            .graph
            .graph
            .neighbors_directed(node, Direction::Incoming)
        {
            if visited.insert(n) {
                if let Some(path) = bundle
                    .graph
                    .graph
                    .node_weight(n)
                    .map(|f| f.relative_path.clone())
                    && path != params.file
                    && max_set.insert(path.clone())
                {
                    max_with_score.push(ScoredPath {
                        path,
                        reason: format!("import_dependent_depth={}", depth + 1),
                        score: 1.0 / (depth as f64 + 2.0),
                    });
                }
                queue.push_back((n, depth + 1));
            }
        }
    }

    // Topic neighbors — chunks sharing the seed's dominant topic.
    let mut topics_present = false;
    if include_topic_neighbors {
        match queries::get_god_file_chunks_with_topics(pool, &params.project, 0).await {
            Ok(chunks) => {
                let seed_topics: HashSet<i64> = chunks
                    .iter()
                    .filter(|c| c.relative_path == params.file)
                    .filter_map(|c| c.topic_id)
                    .collect();
                if !seed_topics.is_empty() {
                    topics_present = true;
                }
                for c in &chunks {
                    if seed_topics.contains(&c.topic_id.unwrap_or(-1))
                        && c.relative_path != params.file
                        && max_set.insert(c.relative_path.clone())
                    {
                        max_with_score.push(ScoredPath {
                            path: c.relative_path.clone(),
                            reason: "same_topic".to_string(),
                            score: c.membership_score.unwrap_or(0.5),
                        });
                    }
                }
            }
            Err(e) => {
                debug!("topic-neighbor fetch failed (non-fatal): {}", e);
            }
        }
    }

    // Verdict by ratio max/recommended.
    let ratio = if recommended_set.is_empty() {
        max_set.len() as f64
    } else {
        max_set.len() as f64 / recommended_set.len() as f64
    };
    let verdict = if ratio < 2.0 {
        "focused"
    } else if ratio < 5.0 {
        "normal"
    } else {
        "sprawling"
    };

    // Sort each set by score desc, path asc.
    sort_scored(&mut min_with_score);
    sort_scored(&mut recommended_with_score);
    sort_scored(&mut max_with_score);

    let result = json!({
        "seed_file": params.file,
        "verdict": verdict,
        "min": min_with_score.iter().map(|s| s.to_json()).collect::<Vec<_>>(),
        "recommended": recommended_with_score.iter().map(|s| s.to_json()).collect::<Vec<_>>(),
        "max": max_with_score.iter().map(|s| s.to_json()).collect::<Vec<_>>(),
        "guidance": match verdict {
            "focused" => "Tight scope — just the seed file and direct importers. Ship as-is.",
            "normal" => "Reasonable scope — include the `recommended` set in this PR.",
            _ => "PR is sprawling. Consider splitting; if not splittable, expand to `max` and \
                  document why so reviewers know what to expect.",
        },
        "health": health_envelope(true, git_history_present, topics_present),
    });
    let json_str = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "pr_scope_recommender",
        verdict,
        min = min_with_score.len(),
        recommended = recommended_with_score.len(),
        max = max_with_score.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json_str)]))
}

#[derive(Debug, Clone)]
struct ScoredPath {
    path: String,
    reason: String,
    score: f64,
}

impl ScoredPath {
    fn to_json(&self) -> serde_json::Value {
        json!({
            "path": self.path,
            "reason": self.reason,
            "score": format!("{:.4}", self.score),
        })
    }
}

fn sort_scored(v: &mut [ScoredPath]) {
    v.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
    });
}

fn health_envelope(
    graph_present: bool,
    git_history_present: bool,
    topics_present: bool,
) -> serde_json::Value {
    json!({
        "graph_stale": !graph_present,
        "git_history_present": git_history_present,
        "topics_present": topics_present,
    })
}

// Stub: a placeholder to test git-history presence by side effect.
// The real check is the existence of any `find_coupled_files` results;
// this trait extension just exists so the body type-checks.
trait DummyExecute {
    async fn execute_dummy(&self, _project: &str) -> Result<bool, sqlx::Error>;
}
impl DummyExecute for sqlx::PgPool {
    async fn execute_dummy(&self, project: &str) -> Result<bool, sqlx::Error> {
        let exists: Option<bool> = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1 FROM git_commit_files gcf
                 JOIN git_commits gc ON gc.id = gcf.commit_id
                 JOIN projects p ON p.id = gc.project_id
                 WHERE p.name = $1
                 LIMIT 1
             )",
        )
        .bind(project)
        .fetch_optional(self)
        .await?;
        Ok(exists.unwrap_or(false))
    }
}
