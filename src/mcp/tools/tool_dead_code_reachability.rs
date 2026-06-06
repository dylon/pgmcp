//! `tool_dead_code_reachability` — Forward closure from roots over
//! `symbol_references` to find unreached private symbols (SOTA Phase 10.1).

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::DeadCodeReachabilityParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

const DEFAULT_LIMIT: i32 = 50;
const MAX_DEAD_CANDIDATES: i32 = 1_000;

fn normalize_limit(limit: Option<i32>) -> usize {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_DEAD_CANDIDATES) as usize
}

pub async fn tool_dead_code_reachability(
    ctx: &SystemContext,
    params: DeadCodeReachabilityParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "dead_code_reachability", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project = params.project.trim().to_string();
    let project_id = project_id_or_err(ctx, &project).await?;
    let pool = pool_or_err(ctx)?;
    let include_tests = params.include_tests.unwrap_or(false);
    let include_bare_name = params.include_bare_name.unwrap_or(false);
    let limit = normalize_limit(params.limit);

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
            "project": project,
            "limit": limit,
            "include_tests": include_tests,
            "include_bare_name": include_bare_name,
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
                $2::bool
                OR (
                    f.relative_path !~ '(^|/)(test|tests|spec|specs)(/|_)'
                    AND f.relative_path !~ '(_test|_spec)\\.[a-z]+$'
                )
           )
           AND (
                COALESCE(fs.visibility, 'private') = 'public'
                OR fs.name IN ('main','start','run','init')
           )",
    )
    .bind(project_id)
    .bind(include_tests)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Roots query failed: {}", e), None))?;

    // Edges: source_symbol_id → target_symbol_id, restricted to
    // high-confidence resolutions (exact_in_file / exact_via_import).
    // Pattern C (resolved-edge consumers) per the shadow-ASR plan: the
    // BFS uses target_path-resolved edges so cross-file dispatch is
    // modeled correctly. Bare-name-resolved edges are admitted only
    // when explicitly opted in via params.include_bare_name (default
    // false) — keeps the dead-code report's false-positive rate low.
    let edges: Vec<(i64, i64)> = sqlx::query_as::<_, (i64, i64)>(
        "SELECT sr.source_symbol_id, sr.target_symbol_id
         FROM symbol_references sr
         JOIN indexed_files sf
           ON sf.id = sr.source_file_id
          AND sf.project_id = $1
         JOIN file_symbols ss
           ON ss.id = sr.source_symbol_id
          AND ss.file_id = sf.id
         JOIN file_symbols ts
           ON ts.id = sr.target_symbol_id
         JOIN indexed_files tf
           ON tf.id = ts.file_id
          AND tf.project_id = $1
         WHERE sr.ref_kind = 'call'
           AND (sr.target_file_id IS NULL OR sr.target_file_id = ts.file_id)
           AND (
                sr.resolution_kind IN ('exact_in_file', 'exact_via_import')
                OR ($2::bool AND sr.resolution_kind = 'bare_name_in_project')
           )",
    )
    .bind(project_id)
    .bind(include_bare_name)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Edge query failed: {}", e), None))?;

    let mut out_edges: HashMap<i64, Vec<i64>> = HashMap::new();
    for (s, t) in edges {
        out_edges.entry(s).or_default().push(t);
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
    let all_syms: Vec<(i64, String, String, i32, String)> = sqlx::query_as::<
        _,
        (i64, String, String, i32, String),
    >(
        "SELECT fs.id, fs.name, f.relative_path, fs.start_line, COALESCE(fs.visibility, 'private')
         FROM file_symbols fs
         JOIN indexed_files f ON fs.file_id = f.id
         WHERE f.project_id = $1
           AND fs.kind IN ('function','class','struct')
           AND (
                $2::bool
                OR (
                    f.relative_path !~ '(^|/)(test|tests|spec|specs)(/|_)'
                    AND f.relative_path !~ '(_test|_spec)\\.[a-z]+$'
                )
           )
         ORDER BY f.relative_path, fs.start_line",
    )
    .bind(project_id)
    .bind(include_tests)
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
        if dead.len() >= limit {
            break;
        }
    }
    // Shadow-ASR channel (Phase D2b): per-effect symbol-count breakdown
    // for the project. Universal enrichment — every tool benefits from
    // surfacing the effect distribution alongside its primary output.
    // Gracefully degrades to empty when shadow-ASR data isn't populated.
    let effect_breakdown: Vec<serde_json::Value> = (async {
        let Some(pool) = ctx.db().pool() else {
            return Vec::new();
        };
        crate::mcp::tools::sema_helpers::effects::effect_counts(pool, project_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
            .collect()
    })
    .await;

    json_result(&json!({
        "effect_breakdown": effect_breakdown,
        "project": project,
        "limit": limit,
        "include_tests": include_tests,
        "include_bare_name": include_bare_name,
        "roots": roots.len(),
        "reached": reached.len(),
        "dead_candidates": dead,
        "health": {
            "symbols_present": true,
        },
        "guidance": "Symbols unreached from roots (main / public exports / entry points) via call edges. False positives possible when callers go through dynamic dispatch / FFI / reflection — verify before deleting."
    }))
}
