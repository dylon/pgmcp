//! `tool_dependency_graph` — MCP tool body, extracted from `super::super::server`.

#![allow(unused_imports)]

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, LoggingLevel};
use serde_json::json;
use tracing::{debug, error, info, warn};

use crate::context::SystemContext;
use crate::mcp::server::*;

pub async fn tool_dependency_graph(
    ctx: &SystemContext,
    params: DependencyGraphParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .dependency_graph_scans
        .fetch_add(1, Ordering::Relaxed);

    let depth = params.depth.unwrap_or(2);
    let format = params.format.as_deref().unwrap_or("summary");
    let edge_type_strs = params
        .edge_types
        .as_deref()
        .map(|v| v.iter().map(|s| s.as_str()).collect::<Vec<_>>())
        .unwrap_or_else(|| vec!["import"]);

    debug!(
        tool = "dependency_graph",
        project = %params.project,
        focus_file = params.focus_file.as_deref().unwrap_or("*"),
        depth,
        format,
        "MCP tool invoked",
    );

    // Resolve project_id
    let project_id: Option<i32> =
        sqlx::query_scalar("SELECT id FROM projects WHERE name = $1")
            .bind(&params.project)
            .fetch_optional(ctx.db().pool().expect(
                "inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>",
            ))
            .await
            .map_err(|e| McpError::internal_error(format!("Project lookup failed: {}", e), None))?;

    let project_id = project_id.ok_or_else(|| {
        McpError::internal_error(format!("Project not found: {}", params.project), None)
    })?;

    // Load edges and file metadata
    #[derive(sqlx::FromRow)]
    #[allow(dead_code)]
    struct EdgeRow {
        source_file_id: i64,
        source_path: String,
        source_lang: String,
        target_file_id: Option<i64>,
        target_path: Option<String>,
        target_lang: Option<String>,
        edge_type: String,
        weight: f64,
    }

    let edges: Vec<EdgeRow> = sqlx::query_as::<_, EdgeRow>(
        "SELECT
            e.source_file_id,
            sf.relative_path as source_path,
            sf.language as source_lang,
            e.target_file_id,
            tf.relative_path as target_path,
            tf.language as target_lang,
            e.edge_type,
            e.weight
         FROM code_graph_edges e
         JOIN indexed_files sf ON e.source_file_id = sf.id
         LEFT JOIN indexed_files tf ON e.target_file_id = tf.id
         WHERE e.project_id = $1",
    )
    .bind(project_id)
    .fetch_all(
        ctx.db()
            .pool()
            .expect("inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>"),
    )
    .await
    .map_err(|e| McpError::internal_error(format!("Edge query failed: {}", e), None))?;

    // Filter by edge types
    let filtered_edges: Vec<&EdgeRow> = edges
        .iter()
        .filter(|e| edge_type_strs.contains(&e.edge_type.as_str()))
        .collect();

    // Collect all nodes
    let mut nodes: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
    for e in &filtered_edges {
        nodes
            .entry(e.source_file_id)
            .or_insert_with(|| e.source_path.clone());
        if let (Some(tid), Some(tp)) = (e.target_file_id, e.target_path.as_ref()) {
            nodes.entry(tid).or_insert_with(|| tp.clone());
        }
    }

    // If focus_file specified, BFS to depth
    let (visible_nodes, visible_edges) = if let Some(ref focus) = params.focus_file {
        let focus_id = nodes
            .iter()
            .find(|(_, path)| path.contains(focus.as_str()))
            .map(|(&id, _)| id);

        if let Some(focus_id) = focus_id {
            // BFS from focus_id
            use std::collections::{HashSet, VecDeque};
            let mut visited: HashSet<i64> = HashSet::new();
            let mut queue: VecDeque<(i64, i32)> = VecDeque::new();
            queue.push_back((focus_id, 0));
            visited.insert(focus_id);

            while let Some((node, d)) = queue.pop_front() {
                if d >= depth {
                    continue;
                }
                // Find neighbors in both directions
                for e in &filtered_edges {
                    if e.source_file_id == node
                        && let Some(tid) = e.target_file_id
                        && visited.insert(tid)
                    {
                        queue.push_back((tid, d + 1));
                    }
                    if e.target_file_id == Some(node) && visited.insert(e.source_file_id) {
                        queue.push_back((e.source_file_id, d + 1));
                    }
                }
            }

            let vis_edges: Vec<&EdgeRow> = filtered_edges
                .iter()
                .filter(|e| {
                    visited.contains(&e.source_file_id)
                        && e.target_file_id
                            .map(|t| visited.contains(&t))
                            .unwrap_or(false)
                })
                .copied()
                .collect();
            let vis_nodes: std::collections::HashMap<i64, String> = nodes
                .into_iter()
                .filter(|(id, _)| visited.contains(id))
                .collect();
            (vis_nodes, vis_edges)
        } else {
            (nodes, filtered_edges)
        }
    } else {
        (nodes, filtered_edges)
    };

    // Count connected components via union-find
    let node_ids: Vec<i64> = visible_nodes.keys().copied().collect();
    let mut id_to_idx: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
    for (i, &id) in node_ids.iter().enumerate() {
        id_to_idx.insert(id, i);
    }
    let mut uf = UnionFind::new(node_ids.len());
    for e in &visible_edges {
        if let (Some(&si), Some(tid)) = (id_to_idx.get(&e.source_file_id), e.target_file_id)
            && let Some(&ti) = id_to_idx.get(&tid)
        {
            uf.union(si, ti);
        }
    }
    let component_count = {
        let mut roots: std::collections::HashSet<usize> = std::collections::HashSet::new();
        for i in 0..node_ids.len() {
            roots.insert(uf.find(i));
        }
        roots.len()
    };

    let result = match format {
        "edges" => {
            let edge_list: Vec<serde_json::Value> = visible_edges
                .iter()
                .map(|e| {
                    serde_json::json!({
                        "source": e.source_path,
                        "target": e.target_path,
                        "edge_type": e.edge_type,
                        "weight": format!("{:.2}", e.weight),
                    })
                })
                .collect();
            serde_json::json!({
                "project": params.project,
                "focus_file": params.focus_file,
                "node_count": visible_nodes.len(),
                "edge_count": visible_edges.len(),
                "components": component_count,
                "edges": edge_list,
            })
        }
        "dot" => {
            let mut dot = String::from(
                "digraph dependencies {\n  rankdir=LR;\n  node [shape=box, fontsize=10];\n",
            );
            for (id, path) in &visible_nodes {
                let short = path.rsplit('/').next().unwrap_or(path);
                dot.push_str(&format!("  n{} [label=\"{}\"];\n", id, short));
            }
            for e in &visible_edges {
                if let Some(tid) = e.target_file_id {
                    let style = match e.edge_type.as_str() {
                        "co_change" => " [style=dashed, color=blue]",
                        "semantic" => " [style=dotted, color=green]",
                        _ => "",
                    };
                    dot.push_str(&format!("  n{} -> n{}{};\n", e.source_file_id, tid, style));
                }
            }
            dot.push_str("}\n");
            serde_json::json!({
                "project": params.project,
                "focus_file": params.focus_file,
                "node_count": visible_nodes.len(),
                "edge_count": visible_edges.len(),
                "components": component_count,
                "dot": dot,
            })
        }
        _ => {
            // summary
            let mut type_counts: std::collections::HashMap<&str, usize> =
                std::collections::HashMap::new();
            for e in &visible_edges {
                *type_counts.entry(&e.edge_type).or_insert(0) += 1;
            }
            serde_json::json!({
                "project": params.project,
                "focus_file": params.focus_file,
                "node_count": visible_nodes.len(),
                "edge_count": visible_edges.len(),
                "components": component_count,
                "edge_type_counts": type_counts,
                "guidance": "Use format: \"edges\" for the full edge list or \"dot\" for Graphviz visualization. \
                             Set focus_file to zoom into a specific file's neighborhood.",
            })
        }
    };

    // Shadow-ASR channel (Phase D2b): per-effect symbol-count breakdown for
    // the project. Lets consumers correlate dependency-graph topology with
    // effect concentration (e.g., is the most-imported file the one
    // carrying all the `unsafe` effects?).
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

    // Cross-project neighborhood (ADR-009 §4.2): the `project_depends_on` edges
    // touching this project — the projects it depends on (which may break it) and
    // the projects that depend on it (which it may break). Surfaced alongside the
    // intra-project import graph so the dependency view spans project boundaries.
    let (cross_project_dependencies, cross_project_dependents) = match ctx.db().pool() {
        Some(pool) => crate::deps::store::cross_project_blocks(pool, project_id).await,
        None => (Vec::new(), Vec::new()),
    };

    let result_with_effects = match result {
        serde_json::Value::Object(mut m) => {
            m.insert(
                "effect_breakdown".to_string(),
                serde_json::json!(effect_breakdown),
            );
            m.insert(
                "cross_project_dependency_count".to_string(),
                serde_json::json!(cross_project_dependencies.len()),
            );
            m.insert(
                "cross_project_dependencies".to_string(),
                serde_json::json!(cross_project_dependencies),
            );
            m.insert(
                "cross_project_dependent_count".to_string(),
                serde_json::json!(cross_project_dependents.len()),
            );
            m.insert(
                "cross_project_dependents".to_string(),
                serde_json::json!(cross_project_dependents),
            );
            serde_json::Value::Object(m)
        }
        other => other,
    };

    let json = serde_json::to_string_pretty(&result_with_effects)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "dependency_graph",
        nodes = visible_nodes.len(),
        edges = visible_edges.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
