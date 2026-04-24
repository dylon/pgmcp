//! `tool_circular_dependencies` — MCP tool body, extracted from `super::super::server`.

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

pub async fn tool_circular_dependencies(
    ctx: &SystemContext,
    params: CircularDependenciesParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().cycle_scans.fetch_add(1, Ordering::Relaxed);

    let max_cycle_length = params.max_cycle_length.unwrap_or(10) as usize;

    info!(
        tool = "circular_dependencies",
        project = %params.project,
        max_cycle_length,
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

    // Load import edges only
    #[derive(sqlx::FromRow)]
    struct EdgeRowDb {
        source_file_id: i64,
        source_relative_path: String,
        source_language: String,
        target_file_id: Option<i64>,
        target_relative_path: Option<String>,
        target_language: Option<String>,
        edge_type: String,
        weight: f64,
    }

    let db_edges: Vec<EdgeRowDb> = sqlx::query_as::<_, EdgeRowDb>(
        "SELECT
            e.source_file_id,
            sf.relative_path as source_relative_path,
            sf.language as source_language,
            e.target_file_id,
            tf.relative_path as target_relative_path,
            tf.language as target_language,
            e.edge_type,
            e.weight
         FROM code_graph_edges e
         JOIN indexed_files sf ON e.source_file_id = sf.id
         LEFT JOIN indexed_files tf ON e.target_file_id = tf.id
         WHERE e.project_id = $1 AND e.edge_type = 'import'",
    )
    .bind(project_id)
    .fetch_all(
        ctx.db()
            .pool()
            .expect("inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>"),
    )
    .await
    .map_err(|e| McpError::internal_error(format!("Edge query failed: {}", e), None))?;

    #[derive(sqlx::FromRow)]
    struct FileMetaDb {
        file_id: i64,
        relative_path: String,
        language: String,
    }

    let file_metas: Vec<FileMetaDb> = sqlx::query_as::<_, FileMetaDb>(
        "SELECT id as file_id, relative_path, language
         FROM indexed_files WHERE project_id = $1",
    )
    .bind(project_id)
    .fetch_all(
        ctx.db()
            .pool()
            .expect("inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>"),
    )
    .await
    .map_err(|e| McpError::internal_error(format!("File query failed: {}", e), None))?;

    use crate::graph::algorithms::{extract_simple_cycles, find_cycles};
    use crate::graph::builder::{FileMetaRow, GraphEdgeRow, build_graph};

    let graph_edges: Vec<GraphEdgeRow> = db_edges
        .iter()
        .map(|e| GraphEdgeRow {
            source_file_id: e.source_file_id,
            source_relative_path: e.source_relative_path.clone(),
            source_language: e.source_language.clone(),
            target_file_id: e.target_file_id,
            target_relative_path: e.target_relative_path.clone(),
            target_language: e.target_language.clone(),
            edge_type: e.edge_type.clone(),
            weight: e.weight,
        })
        .collect();

    let metas: Vec<FileMetaRow> = file_metas
        .iter()
        .map(|f| FileMetaRow {
            file_id: f.file_id,
            relative_path: f.relative_path.clone(),
            language: f.language.clone(),
        })
        .collect();

    let code_graph = build_graph(&graph_edges, &metas);
    let sccs = find_cycles(&code_graph.graph);

    let mut all_cycles: Vec<serde_json::Value> = Vec::new();
    for scc in &sccs {
        let simple = extract_simple_cycles(&code_graph.graph, scc, max_cycle_length);
        for cycle in &simple {
            let paths: Vec<&str> = cycle
                .iter()
                .filter_map(|n| {
                    code_graph
                        .graph
                        .node_weight(*n)
                        .map(|f| f.relative_path.as_str())
                })
                .collect();
            all_cycles.push(serde_json::json!({
                "length": cycle.len(),
                "files": paths,
            }));
        }
    }

    all_cycles.sort_by_key(|c| c["length"].as_u64().unwrap_or(0));

    let result = serde_json::json!({
        "project": params.project,
        "max_cycle_length": max_cycle_length,
        "scc_count": sccs.len(),
        "cycle_count": all_cycles.len(),
        "cycles": all_cycles,
        "guidance": "Circular dependencies increase build times and coupling. \
                     Break cycles by introducing interfaces, dependency inversion, \
                     or restructuring modules. Shortest cycles are easiest to fix first.",
    });

    let json = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "circular_dependencies",
        sccs = sccs.len(),
        cycles = all_cycles.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
