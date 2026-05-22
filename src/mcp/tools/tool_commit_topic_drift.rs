//! `tool_commit_topic_drift` — TF-IDF over sliding windows of commit messages
//! (SOTA Phase 11.3). Reports files whose recent commit-message vocabulary
//! has drifted from the prior window.

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use rust_stemmers::{Algorithm, Stemmer};
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::CommitTopicDriftParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

fn tokenize(s: &str, stemmer: &Stemmer) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 3 && !t.chars().all(|c| c.is_numeric()))
        .map(|t| stemmer.stem(&t.to_lowercase()).to_string())
        .collect()
}

fn cosine(a: &HashMap<String, f64>, b: &HashMap<String, f64>) -> f64 {
    let mut dot = 0.0;
    for (k, av) in a {
        if let Some(bv) = b.get(k) {
            dot += av * bv;
        }
    }
    let na: f64 = a.values().map(|v| v * v).sum::<f64>().sqrt();
    let nb: f64 = b.values().map(|v| v * v).sum::<f64>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

pub async fn tool_commit_topic_drift(
    ctx: &SystemContext,
    params: CommitTopicDriftParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "commit_topic_drift", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let window = params.window_commits.unwrap_or(20).max(2) as usize;
    let limit = params.limit.unwrap_or(30);

    // Fetch per-file commit subjects in chronological order.
    let rows: Vec<(String, String, String)> = sqlx::query_as::<_, (String, String, String)>(
        "SELECT gcf.file_path, gc.subject, COALESCE(gc.body, '')
         FROM git_commits gc
         JOIN git_commit_files gcf ON gcf.commit_id = gc.id
         WHERE gc.project_id = $1
         ORDER BY gc.committed_at",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Commit query failed: {}", e), None))?;

    let mut per_file: HashMap<String, Vec<String>> = HashMap::new();
    for (path, subj, body) in rows {
        per_file
            .entry(path)
            .or_default()
            .push(format!("{} {}", subj, body));
    }
    let stemmer = Stemmer::create(Algorithm::English);

    let mut drift_rows: Vec<(String, f64, usize)> = Vec::new();
    for (path, msgs) in per_file {
        if msgs.len() < 2 * window {
            continue;
        }
        let n = msgs.len();
        let prev = &msgs[(n - 2 * window)..(n - window)];
        let cur = &msgs[(n - window)..];
        let mut prev_freq: HashMap<String, f64> = HashMap::new();
        let mut cur_freq: HashMap<String, f64> = HashMap::new();
        for m in prev {
            for t in tokenize(m, &stemmer) {
                *prev_freq.entry(t).or_insert(0.0) += 1.0;
            }
        }
        for m in cur {
            for t in tokenize(m, &stemmer) {
                *cur_freq.entry(t).or_insert(0.0) += 1.0;
            }
        }
        if prev_freq.is_empty() || cur_freq.is_empty() {
            continue;
        }
        let cos = cosine(&prev_freq, &cur_freq);
        let drift = 1.0 - cos;
        drift_rows.push((path, drift, n));
    }
    drift_rows.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    drift_rows.truncate(limit.max(0) as usize);
    let rows_json: Vec<_> = drift_rows
        .iter()
        .map(|(p, d, n)| json!({"file": p, "drift": d, "total_commits": n}))
        .collect();
    json_result(&json!({
        "project": params.project,
        "window_commits": window,
        "files": rows_json,
        "guidance": "1 − cosine between consecutive commit-message windows (Porter-stemmed). High drift indicates the file's purpose/concerns shifted in the recent window."
    }))
}
