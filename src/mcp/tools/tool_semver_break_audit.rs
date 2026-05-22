//! `tool_semver_break_audit` — Detect REMOVED public symbols across git
//! history (SOTA Phase 7.2). Compares the current public-API surface to the
//! one at `base_ref` commits ago.

#![allow(unused_imports)]

use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::collections::HashSet;
use std::sync::atomic::Ordering;
use strsim::levenshtein;

use crate::context::SystemContext;
use crate::mcp::server::SemverBreakAuditParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_semver_break_audit(
    ctx: &SystemContext,
    params: SemverBreakAuditParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "semver_break_audit", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;

    // Current public API snapshot.
    let now: Vec<(String, String, String)> = sqlx::query_as::<_, (String, String, String)>(
        "SELECT f.relative_path, fs.name, fs.kind
         FROM file_symbols fs
         JOIN indexed_files f ON fs.file_id = f.id
         WHERE f.project_id = $1 AND fs.visibility = 'public'",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("API query failed: {}", e), None))?;
    let now_set: HashSet<(String, String, String)> = now.into_iter().collect();

    // Build a "previous public API" candidate set by scanning the commit-chunk
    // text from the last N commits for public-marker patterns (Rust `pub fn` /
    // Python top-level `def` / JS `export`).
    let window = params.window_commits.unwrap_or(50) as i64;
    let candidate_rows: Vec<(String,)> = sqlx::query_as::<_, (String,)>(
        "SELECT gcc.chunk_text
         FROM git_commits gc
         JOIN git_commit_chunks gcc ON gcc.commit_id = gc.id
         WHERE gc.project_id = $1
         ORDER BY gc.committed_at DESC
         LIMIT $2",
    )
    .bind(project_id)
    .bind(window)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let pub_re = Regex::new(r"(?m)\bpub(?:\(crate\))?\s+(fn|struct|enum|trait|const|static|type)\s+([A-Za-z_][A-Za-z0-9_]*)|\bexport\s+(function|class|const|let|var|interface|enum|type)\s+([A-Za-z_][A-Za-z0-9_]*)|^def\s+([A-Za-z_][A-Za-z0-9_]*)").expect("pub regex");
    let mut historical: HashSet<(String, String)> = HashSet::new();
    for (text,) in &candidate_rows {
        for cap in pub_re.captures_iter(text) {
            let (kind, name) = if let (Some(k), Some(n)) = (cap.get(1), cap.get(2)) {
                (k.as_str().to_string(), n.as_str().to_string())
            } else if let (Some(k), Some(n)) = (cap.get(3), cap.get(4)) {
                (k.as_str().to_string(), n.as_str().to_string())
            } else if let Some(n) = cap.get(5) {
                ("function".to_string(), n.as_str().to_string())
            } else {
                continue;
            };
            historical.insert((kind, name));
        }
    }
    // Removed = in historical but not in now.
    let mut removed: Vec<(String, String, Option<String>)> = Vec::new();
    let now_names: HashSet<String> = now_set.iter().map(|(_, n, _)| n.clone()).collect();
    for (kind, name) in &historical {
        if !now_names.contains(name) {
            // Possible rename: nearest name in now_names by Levenshtein.
            let mut best: Option<(String, usize)> = None;
            for n in &now_names {
                let d = levenshtein(name, n);
                if best.as_ref().map(|(_, bd)| d < *bd).unwrap_or(true) {
                    best = Some((n.clone(), d));
                }
            }
            let likely_rename = best.filter(|(_, d)| *d <= 2).map(|(n, _)| n);
            removed.push((kind.clone(), name.clone(), likely_rename));
        }
    }
    let limit = params.limit.unwrap_or(50) as usize;
    removed.truncate(limit);
    let rows_json: Vec<_> = removed
        .into_iter()
        .map(|(k, n, r)| {
            json!({
                "kind": k,
                "name": n,
                "likely_rename_to": r,
                "severity": if r.is_some() { "major (renamed)" } else { "major (removed)" },
            })
        })
        .collect();
    json_result(&json!({
        "project": params.project,
        "window_commits": window,
        "removed_or_renamed": rows_json,
        "guidance": "Removed/renamed public symbols are major-version breakages under semver. Rename candidates within Levenshtein <= 2 are flagged for clarification."
    }))
}
