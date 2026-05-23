//! `tool_lcom4` — Hitz-Montazeri 1995 (corrected LCOM) per class/module
//! (SOTA Phase 10.4). LCOM4 = number of connected components in the
//! method-field-access graph.
//!
//! Approximation: per file, count distinct call-graph SCC-mate clusters of
//! functions sharing common call targets. High LCOM4 = god-class candidate.

#![allow(unused_imports)]

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::Lcom4Params;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_lcom4(
    ctx: &SystemContext,
    params: Lcom4Params,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "lcom4", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;
    let limit = params.limit.unwrap_or(30);

    // For each container (class/struct/trait/interface), collect the call
    // targets of its functions and count connected components of those
    // functions in the shared-target graph.
    let rows: Vec<(i64, String, String, String)> =
        sqlx::query_as::<_, (i64, String, String, String)>(
            "SELECT fs.id, fs.name AS container, f.relative_path, COALESCE(child.target_raws, '')
         FROM file_symbols fs
         JOIN indexed_files f ON fs.file_id = f.id
         JOIN LATERAL (
            SELECT STRING_AGG(DISTINCT sr.target_raw, '|') AS target_raws,
                   ARRAY_AGG(DISTINCT fs2.id) AS member_ids
            FROM file_symbols fs2
            LEFT JOIN symbol_references sr
                ON sr.source_symbol_id = fs2.id AND sr.ref_kind = 'call'
            WHERE fs2.parent_id = fs.id AND fs2.kind = 'function'
         ) child ON TRUE
         WHERE f.project_id = $1
           AND fs.kind IN ('class','struct','trait','interface')",
        )
        .bind(project_id)
        .fetch_all(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("LCOM4 query failed: {}", e), None))?;

    // For each container, compute number of components by partitioning member
    // functions on shared call targets.
    let mut findings: Vec<(String, String, u32, u32)> = Vec::new();
    for (cid, container, path, _targets) in rows {
        // Fetch members + their call targets.
        let members: Vec<(i64, String, Vec<String>)> = sqlx::query_as::<_, (i64, String, Vec<String>)>(
            "SELECT fs.id, fs.name,
                    COALESCE(ARRAY_AGG(sr.target_raw) FILTER (WHERE sr.target_raw IS NOT NULL), '{}')::text[]
             FROM file_symbols fs
             LEFT JOIN symbol_references sr ON sr.source_symbol_id = fs.id AND sr.ref_kind = 'call'
             WHERE fs.parent_id = $1 AND fs.kind = 'function'
             GROUP BY fs.id, fs.name",
        )
        .bind(cid)
        .fetch_all(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("Member fetch failed: {}", e), None))?;
        if members.len() < 2 {
            continue;
        }
        // Build adjacency on shared target.
        let mut adj: HashMap<i64, HashSet<i64>> = HashMap::new();
        for (a_id, _, a_targets) in &members {
            adj.entry(*a_id).or_default();
            let a_set: HashSet<&str> = a_targets.iter().map(|s| s.as_str()).collect();
            for (b_id, _, b_targets) in &members {
                if a_id == b_id {
                    continue;
                }
                if b_targets.iter().any(|t| a_set.contains(t.as_str())) {
                    adj.entry(*a_id).or_default().insert(*b_id);
                }
            }
        }
        // Count components via BFS.
        let mut visited: HashSet<i64> = HashSet::new();
        let mut components: u32 = 0;
        for (id, _, _) in &members {
            if visited.contains(id) {
                continue;
            }
            components += 1;
            let mut q: VecDeque<i64> = VecDeque::new();
            q.push_back(*id);
            visited.insert(*id);
            while let Some(v) = q.pop_front() {
                if let Some(nbrs) = adj.get(&v) {
                    for &nb in nbrs {
                        if visited.insert(nb) {
                            q.push_back(nb);
                        }
                    }
                }
            }
        }
        if components >= 2 {
            findings.push((path, container, members.len() as u32, components));
        }
    }
    findings.sort_by_key(|a| std::cmp::Reverse(a.3));
    findings.truncate(limit.max(0) as usize);
    let rows_json: Vec<_> = findings
        .iter()
        .map(|(p, c, n, lc)| {
            json!({
                "file": p,
                "container": c,
                "members": n,
                "lcom4": lc,
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

    json_result(&json!({
        "effect_breakdown": effect_breakdown,
        "project": params.project,
        "containers": rows_json,
        "guidance": "LCOM4 = connected-component count in the member-method shared-target graph. ≥2 indicates a class doing multiple unrelated things — god-class candidate."
    }))
}
