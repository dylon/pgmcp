//! `tool_dependency_graph` — MCP tool body, extracted from `super::super::server`.

#![allow(unused_imports)]

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, LoggingLevel};
use serde_json::json;
use tracing::debug;

use crate::context::SystemContext;
use crate::mcp::server::*;
use crate::mcp::tools::sota_helpers::{pool_or_err, project_id_or_err};

const MAX_DEPENDENCY_GRAPH_DEPTH: i32 = 8;
const MAX_DEPENDENCY_GRAPH_OUTPUT_EDGES: usize = 2_000;
const MAX_DEPENDENCY_GRAPH_DOT_NODES: usize = 2_000;
const ALLOWED_EDGE_TYPES: &[&str] = &["import", "co_change", "semantic"];

fn validate_dependency_graph_format(format: Option<String>) -> Result<String, McpError> {
    let format = format.unwrap_or_else(|| "summary".to_string());
    let format = format.trim();
    match format {
        "summary" | "edges" | "dot" => Ok(format.to_string()),
        _ => Err(McpError::invalid_params(
            "format must be one of: summary, edges, dot",
            None,
        )),
    }
}

fn validate_dependency_graph_edge_types(
    edge_types: Option<Vec<String>>,
) -> Result<Vec<String>, McpError> {
    let Some(edge_types) = edge_types else {
        return Ok(vec!["import".to_string()]);
    };
    if edge_types.is_empty() {
        return Err(McpError::invalid_params(
            "edge_types must not be empty",
            None,
        ));
    }

    let mut out = Vec::with_capacity(edge_types.len());
    for edge_type in edge_types {
        let edge_type = edge_type.trim();
        if !ALLOWED_EDGE_TYPES.contains(&edge_type) {
            return Err(McpError::invalid_params(
                format!(
                    "edge_type '{}' is invalid; expected one of: import, co_change, semantic",
                    edge_type
                ),
                None,
            ));
        }
        out.push(edge_type.to_string());
    }
    out.sort();
    out.dedup();
    Ok(out)
}

fn normalize_focus_file(focus_file: Option<String>) -> Result<Option<String>, McpError> {
    match focus_file {
        Some(focus) => {
            let focus = focus.trim();
            if focus.is_empty() {
                Err(McpError::invalid_params(
                    "focus_file must not be blank",
                    None,
                ))
            } else {
                Ok(Some(focus.to_string()))
            }
        }
        None => Ok(None),
    }
}

fn escape_dot_label(label: &str) -> String {
    label.replace('\\', "\\\\").replace('"', "\\\"")
}

pub async fn tool_dependency_graph(
    ctx: &SystemContext,
    params: DependencyGraphParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats()
        .dependency_graph_scans
        .fetch_add(1, Ordering::Relaxed);

    let project = params.project.trim().to_string();
    if project.is_empty() {
        return Err(McpError::invalid_params("project must be non-empty", None));
    }
    let depth = params
        .depth
        .unwrap_or(2)
        .clamp(0, MAX_DEPENDENCY_GRAPH_DEPTH);
    let format = validate_dependency_graph_format(params.format)?;
    let edge_type_strs = validate_dependency_graph_edge_types(params.edge_types)?;
    let focus_file = normalize_focus_file(params.focus_file)?;
    let pool = pool_or_err(ctx)?;
    let project_id = project_id_or_err(ctx, &project).await?;

    debug!(
        tool = "dependency_graph",
        project = %project,
        focus_file = focus_file.as_deref().unwrap_or("*"),
        depth,
        format,
        "MCP tool invoked",
    );

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
                            AND sf.project_id = e.project_id
         LEFT JOIN indexed_files tf ON e.target_file_id = tf.id
                                  AND tf.project_id = e.project_id
         WHERE e.project_id = $1
           AND e.edge_type = ANY($2::text[])
           AND (e.target_file_id IS NULL OR tf.id IS NOT NULL)",
    )
    .bind(project_id)
    .bind(&edge_type_strs)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Edge query failed: {}", e), None))?;

    // Collect all nodes
    let mut nodes: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
    for e in &edges {
        nodes
            .entry(e.source_file_id)
            .or_insert_with(|| e.source_path.clone());
        if let (Some(tid), Some(tp)) = (e.target_file_id, e.target_path.as_ref()) {
            nodes.entry(tid).or_insert_with(|| tp.clone());
        }
    }

    let focus_row: Option<(i64, String)> = match focus_file.as_deref() {
        Some(focus) => {
            let row = sqlx::query_as::<_, (i64, String)>(
                "SELECT id, relative_path
                 FROM indexed_files
                 WHERE project_id = $1 AND relative_path = $2
                 ORDER BY id
                 LIMIT 1",
            )
            .bind(project_id)
            .bind(focus)
            .fetch_optional(pool)
            .await
            .map_err(|e| McpError::internal_error(format!("Focus lookup failed: {}", e), None))?;
            Some(row.ok_or_else(|| {
                McpError::invalid_params(format!("focus_file not found: {}", focus), None)
            })?)
        }
        None => None,
    };
    if let Some((focus_id, focus_path)) = &focus_row {
        nodes.entry(*focus_id).or_insert_with(|| focus_path.clone());
    }

    // If focus_file specified, BFS to depth
    let (visible_nodes, visible_edges) = if let Some((focus_id, _)) = focus_row {
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
            for e in &edges {
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

        let vis_edges: Vec<&EdgeRow> = edges
            .iter()
            .filter(|e| {
                visited.contains(&e.source_file_id)
                    && e.target_file_id
                        .map(|t| visited.contains(&t))
                        .unwrap_or(false)
            })
            .collect();
        let vis_nodes: std::collections::HashMap<i64, String> = nodes
            .into_iter()
            .filter(|(id, _)| visited.contains(id))
            .collect();
        (vis_nodes, vis_edges)
    } else {
        (nodes, edges.iter().collect())
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

    let total_visible_edges = visible_edges.len();
    let edge_output_truncated = total_visible_edges > MAX_DEPENDENCY_GRAPH_OUTPUT_EDGES;
    let result = match format.as_str() {
        "edges" => {
            let edge_list: Vec<serde_json::Value> = visible_edges
                .iter()
                .take(MAX_DEPENDENCY_GRAPH_OUTPUT_EDGES)
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
                "project": project,
                "focus_file": focus_file,
                "depth": depth,
                "edge_types": edge_type_strs,
                "node_count": visible_nodes.len(),
                "edge_count": visible_edges.len(),
                "components": component_count,
                "edges_truncated": edge_output_truncated,
                "reported_edge_count": edge_list.len(),
                "edges": edge_list,
            })
        }
        "dot" => {
            let mut dot = String::from(
                "digraph dependencies {\n  rankdir=LR;\n  node [shape=box, fontsize=10];\n",
            );
            let mut emitted_nodes = std::collections::HashSet::new();
            for (id, path) in visible_nodes.iter().take(MAX_DEPENDENCY_GRAPH_DOT_NODES) {
                let short = path.rsplit('/').next().unwrap_or(path);
                dot.push_str(&format!(
                    "  n{} [label=\"{}\"];\n",
                    id,
                    escape_dot_label(short)
                ));
                emitted_nodes.insert(*id);
            }
            let mut emitted_edges = 0usize;
            for e in &visible_edges {
                if emitted_edges >= MAX_DEPENDENCY_GRAPH_OUTPUT_EDGES {
                    break;
                }
                if let Some(tid) = e.target_file_id {
                    if !emitted_nodes.contains(&e.source_file_id) || !emitted_nodes.contains(&tid) {
                        continue;
                    }
                    let style = match e.edge_type.as_str() {
                        "co_change" => " [style=dashed, color=blue]",
                        "semantic" => " [style=dotted, color=green]",
                        _ => "",
                    };
                    dot.push_str(&format!("  n{} -> n{}{};\n", e.source_file_id, tid, style));
                    emitted_edges += 1;
                }
            }
            dot.push_str("}\n");
            serde_json::json!({
                "project": project,
                "focus_file": focus_file,
                "depth": depth,
                "edge_types": edge_type_strs,
                "node_count": visible_nodes.len(),
                "edge_count": visible_edges.len(),
                "components": component_count,
                "dot_truncated": visible_nodes.len() > MAX_DEPENDENCY_GRAPH_DOT_NODES
                    || edge_output_truncated,
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
                "project": project,
                "focus_file": focus_file,
                "depth": depth,
                "edge_types": edge_type_strs,
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
    let effect_breakdown: Vec<serde_json::Value> =
        crate::mcp::tools::sema_helpers::effects::effect_counts(pool, project_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
            .collect();

    // Cross-project neighborhood (ADR-009 §4.2): the `project_depends_on` edges
    // touching this project — the projects it depends on (which may break it) and
    // the projects that depend on it (which it may break). Surfaced alongside the
    // intra-project import graph so the dependency view spans project boundaries.
    let (cross_project_dependencies, cross_project_dependents) =
        crate::deps::store::cross_project_blocks(pool, project_id).await;

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
