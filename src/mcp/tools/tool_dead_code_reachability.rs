//! `tool_dead_code_reachability` — Forward closure from roots over
//! `symbol_references` to find unreached private symbols (SOTA Phase 10.1).

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::DeadCodeReachabilityParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_dead_code_reachability(
    ctx: &SystemContext,
    params: DeadCodeReachabilityParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "dead_code_reachability", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let include_tests = params.include_tests.unwrap_or(false);
    let limit = params.limit.unwrap_or(50);

    // Pre-flight: if no symbols have been extracted yet, return a
    // structured soft-fail mirroring `naming_consistency`'s pattern.
    // The symbol-extraction cron has a 30-min Ready-relative delay and
    // a 2-h interval by default, so freshly-started daemons can return
    // `reached: 0, dead_candidates: []` without this guard — which is
    // indistinguishable from "everything is reachable". The soft-fail
    // makes the unready state explicit.
    let symbol_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM file_symbols fs \
         JOIN indexed_files f ON fs.file_id = f.id \
         WHERE f.project_id = $1",
    )
    .bind(project_id)
    .fetch_one(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Symbol pre-flight failed: {e}"), None))?;

    if symbol_count == 0 {
        return json_result(&json!({
            "project": params.project,
            "roots": 0,
            "reached": 0,
            "dead_candidates": [],
            "health": {
                "symbols_present": false,
            },
            "guidance": "No symbols extracted for this project yet. The symbol-extraction \
                         cron runs 30 min after Ready and every 2 h thereafter; until it has \
                         populated `file_symbols` + `symbol_references`, dead-code reachability \
                         cannot distinguish 'no callers' from 'no data'. Trigger an immediate \
                         run via `trigger_cron job=\"symbol-extraction\"` (and `call-graph` \
                         afterwards) to populate now."
        }));
    }

    // Roots: public symbols + main / start / entry-point names.
    let roots: Vec<(i64, String, String)> = sqlx::query_as::<_, (i64, String, String)>(
        "SELECT fs.id, fs.name, COALESCE(fs.visibility, 'private')
         FROM file_symbols fs
         JOIN indexed_files f ON fs.file_id = f.id
         WHERE f.project_id = $1
           AND (
                COALESCE(fs.visibility, 'private') = 'public'
                OR fs.name IN ('main','start','run','init')
           )",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Roots query failed: {}", e), None))?;

    // Edges: source_symbol_id → target_symbol_id (call edges).
    let edges: Vec<(Option<i64>, Option<i64>)> = sqlx::query_as::<_, (Option<i64>, Option<i64>)>(
        "SELECT sr.source_symbol_id, sr.target_symbol_id
         FROM symbol_references sr
         JOIN indexed_files f ON sr.source_file_id = f.id
         WHERE f.project_id = $1 AND sr.ref_kind = 'call'",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Edge query failed: {}", e), None))?;

    let mut out_edges: HashMap<i64, Vec<i64>> = HashMap::new();
    for (s, t) in edges {
        if let (Some(s), Some(t)) = (s, t) {
            out_edges.entry(s).or_default().push(t);
        }
    }

    // BFS from roots.
    let mut reached: HashSet<i64> = HashSet::new();
    let mut q: VecDeque<i64> = VecDeque::new();
    for (sid, _name, _vis) in &roots {
        if reached.insert(*sid) {
            q.push_back(*sid);
        }
    }
    while let Some(v) = q.pop_front() {
        if let Some(ts) = out_edges.get(&v) {
            for &t in ts {
                if reached.insert(t) {
                    q.push_back(t);
                }
            }
        }
    }

    // Unreached private symbols = dead candidates.
    let test_clause = if include_tests {
        ""
    } else {
        " AND f.relative_path !~ '(^|/)(test|tests|spec|specs)(/|_)' AND f.relative_path !~ '(_test|_spec)\\.[a-z]+$'"
    };
    let sql = format!(
        "SELECT fs.id, fs.name, f.relative_path, fs.start_line, COALESCE(fs.visibility, 'private')
         FROM file_symbols fs
         JOIN indexed_files f ON fs.file_id = f.id
         WHERE f.project_id = $1
           AND fs.kind IN ('function','class','struct')
           {test_clause}
         ORDER BY f.relative_path, fs.start_line"
    );
    let all_syms: Vec<(i64, String, String, i32, String)> =
        sqlx::query_as::<_, (i64, String, String, i32, String)>(&sql)
            .bind(project_id)
            .fetch_all(pool)
            .await
            .map_err(|e| McpError::internal_error(format!("Symbol enum failed: {}", e), None))?;

    let mut dead: Vec<serde_json::Value> = Vec::new();
    for (id, name, path, line, vis) in all_syms {
        if reached.contains(&id) {
            continue;
        }
        dead.push(json!({
            "name": name,
            "file": path,
            "start_line": line,
            "visibility": vis,
        }));
        if dead.len() >= limit.max(0) as usize {
            break;
        }
    }
    json_result(&json!({
        "project": params.project,
        "roots": roots.len(),
        "reached": reached.len(),
        "dead_candidates": dead,
        "health": {
            "symbols_present": true,
        },
        "guidance": "Symbols unreached from roots (main / public exports / entry points) via call edges. False positives possible when callers go through dynamic dispatch / FFI / reflection — verify before deleting."
    }))
}
