//! `tool_deadlock_candidates` — Cycles in the lock-order graph (SOTA Phase 5.4, Havender 1968).
//!
//! Walks function bodies for lock-acquire sequences `lock(A); lock(B)`, builds
//! a directed graph of lock-order pairs, and reports any SCC of size >= 2 as
//! a potential deadlock candidate.

#![allow(unused_imports)]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;

use petgraph::graph::{DiGraph, NodeIndex};
use regex::Regex;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::context::SystemContext;
use crate::mcp::server::DeadlockCandidatesParams;
use crate::mcp::tools::sota_helpers::{json_result, pool_or_err, project_id_or_err};

pub async fn tool_deadlock_candidates(
    ctx: &SystemContext,
    params: DeadlockCandidatesParams,
) -> Result<CallToolResult, McpError> {
    tracing::debug!(tool = "deadlock_candidates", "MCP tool invoked");
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    let project_id = project_id_or_err(ctx, &params.project).await?;
    let pool = pool_or_err(ctx)?;

    let rows: Vec<(String, Option<String>)> = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT relative_path, content FROM indexed_files WHERE project_id = $1 AND content IS NOT NULL",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("File scan failed: {}", e), None))?;

    // Match `<ident>.lock()`, `Mutex<…>::lock`, `lock(<ident>)`, etc.
    let acquire_re = Regex::new(r"(?m)\b([A-Za-z_][A-Za-z0-9_]*)\s*\.\s*(lock|read|write)\s*\(")
        .expect("lock acquire regex");
    let body_re =
        Regex::new(r"(?ms)\bfn\s+[A-Za-z_][A-Za-z0-9_]*\s*[^{]*\{").expect("fn body regex");

    let mut order_pairs: HashMap<(String, String), u32> = HashMap::new();
    for (_path, content) in &rows {
        let Some(c) = content.as_deref() else {
            continue;
        };
        for fb in body_re.find_iter(c) {
            let start = fb.end();
            // Walk forward until matching brace closes the function.
            let mut depth = 1i32;
            let mut end = c.len();
            for (i, ch) in c[start..].char_indices() {
                if ch == '{' {
                    depth += 1;
                } else if ch == '}' {
                    depth -= 1;
                    if depth == 0 {
                        end = start + i;
                        break;
                    }
                }
            }
            let body = &c[start..end];
            let acquires: Vec<&str> = acquire_re
                .captures_iter(body)
                .filter_map(|m| m.get(1).map(|x| x.as_str()))
                .collect();
            for window in acquires.windows(2) {
                let (a, b) = (window[0], window[1]);
                if a != b {
                    *order_pairs
                        .entry((a.to_string(), b.to_string()))
                        .or_insert(0) += 1;
                }
            }
        }
    }

    // Build a graph and find SCCs.
    let mut node_of: HashMap<String, NodeIndex> = HashMap::new();
    let mut g: DiGraph<String, u32> = DiGraph::new();
    for ((a, b), w) in &order_pairs {
        let na = *node_of
            .entry(a.clone())
            .or_insert_with(|| g.add_node(a.clone()));
        let nb = *node_of
            .entry(b.clone())
            .or_insert_with(|| g.add_node(b.clone()));
        g.add_edge(na, nb, *w);
    }
    let sccs = petgraph::algo::tarjan_scc(&g);
    let cycles: Vec<Vec<String>> = sccs
        .into_iter()
        .filter(|c| c.len() >= 2)
        .map(|c| {
            c.into_iter()
                .filter_map(|ni| g.node_weight(ni).cloned())
                .collect()
        })
        .collect();
    json_result(&json!({
        "project": params.project,
        "cycles": cycles,
        "edges": order_pairs.iter().map(|((a, b), w)| json!({"from": a, "to": b, "weight": w})).collect::<Vec<_>>(),
        "guidance": "Cycles in the lock-order graph indicate that two code paths acquire the same locks in reverse order — a textbook Havender 1968 deadlock recipe."
    }))
}
