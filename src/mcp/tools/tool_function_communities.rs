//! `tool_function_communities` — Louvain communities over the call graph
//! (graph-roadmap Phase 1.1).
//!
//! Reads `function_metrics.community_id`, assigned by running the genericized
//! Louvain algorithm on the symbol-resolved call graph in the call-graph cron.
//! Answers "what are the natural functional clusters / de-facto execution
//! layers, independent of the file and directory layout?" — communities often
//! cut across files, revealing the real modular structure vs. the on-disk one.

use std::collections::BTreeMap;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::context::SystemContext;
use crate::mcp::server::FunctionCommunitiesParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

/// One community member: (file, function, start_line, end_line, pagerank, fan_in, fan_out).
type CommunityMember = (String, String, i32, i32, f64, i32, i32);

pub async fn tool_function_communities(
    ctx: &SystemContext,
    params: FunctionCommunitiesParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "function_communities", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;

    let min_size = params.min_size.unwrap_or(2).max(1) as usize;
    let max_communities = params.limit.unwrap_or(30).clamp(1, 500) as usize;
    let members_cap = params.members_per_community.unwrap_or(15).clamp(1, 200) as usize;

    #[allow(clippy::type_complexity)]
    let rows: Vec<(i32, String, String, i32, i32, f64, i32, i32)> = sqlx::query_as(
        "SELECT fm.community_id, f.relative_path, fs.name, fs.start_line, fs.end_line,
                fm.pagerank, fm.fan_in, fm.fan_out
         FROM function_metrics fm
         JOIN file_symbols fs ON fs.id = fm.function_id
         JOIN indexed_files f ON f.id = fs.file_id
         WHERE fm.project_id = $1 AND fm.community_id >= 0
         ORDER BY fm.community_id, fm.pagerank DESC",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| {
        McpError::internal_error(format!("function_communities query failed: {}", e), None)
    })?;

    // Group members by community id (rows already sorted by pagerank within id).
    let mut groups: BTreeMap<i32, Vec<CommunityMember>> = BTreeMap::new();
    for (cid, path, name, s, e, pr, fi, fo) in rows {
        groups
            .entry(cid)
            .or_default()
            .push((path, name, s, e, pr, fi, fo));
    }

    let mut communities: Vec<serde_json::Value> = groups
        .into_iter()
        .filter(|(_, members)| members.len() >= min_size)
        .map(|(cid, members)| {
            let size = members.len();
            // Distinct files the community spans — high file-spread means the
            // cluster is scattered across the on-disk layout.
            let mut files: Vec<&str> = members.iter().map(|m| m.0.as_str()).collect();
            files.sort_unstable();
            files.dedup();
            let file_spread = files.len();
            let top_members: Vec<_> = members
                .iter()
                .take(members_cap)
                .map(|(path, name, s, e, pr, fi, fo)| {
                    json!({
                        "file": path, "function": name, "start_line": s, "end_line": e,
                        "pagerank": pr, "fan_in": fi, "fan_out": fo,
                    })
                })
                .collect();
            json!({
                "community_id": cid,
                "size": size,
                "file_spread": file_spread,
                "members": top_members,
            })
        })
        .collect();

    // Largest communities first.
    communities.sort_by(|a, b| {
        b["size"]
            .as_u64()
            .unwrap_or(0)
            .cmp(&a["size"].as_u64().unwrap_or(0))
    });
    let total_communities = communities.len();
    communities.truncate(max_communities);

    json_result(&json!({
        "project": params.project,
        "total_communities": total_communities,
        "communities": communities,
        "guidance": if total_communities == 0 {
            "No communities found — the `call-graph` cron has not assigned community_id for this \
             project yet (community_id = -1 until it runs). Ensure `symbol-extraction` and \
             `function-metrics` ran, then trigger `call-graph` and retry."
        } else {
            "Each community is a cluster of functions densely connected by calls. A community with \
             high `file_spread` is a feature/concern smeared across many files (a candidate for \
             colocation); a single large community spanning most of the project may indicate weak \
             modular boundaries. Compare against the directory layout to find where the code's real \
             structure and its filesystem structure disagree."
        }
    }))
}
