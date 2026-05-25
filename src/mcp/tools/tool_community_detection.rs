//! `tool_community_detection` — MCP tool body, extracted from `super::super::server`.

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

pub async fn tool_community_detection(
    ctx: &SystemContext,
    params: CommunityDetectionParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().community_scans.fetch_add(1, Ordering::Relaxed);

    let graph_type = params.graph_type.as_deref().unwrap_or("import");
    let resolution = params.resolution.unwrap_or(1.0);

    debug!(
        tool = "community_detection",
        project = %params.project,
        graph_type,
        resolution,
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

    // Load edges
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

    let edge_type_filter = match graph_type {
        "co_change" => "AND e.edge_type = 'co_change'",
        "import" => "AND e.edge_type = 'import'",
        _ => "", // combined: all edge types
    };

    let query = format!(
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
         WHERE e.project_id = $1 {}",
        edge_type_filter
    );

    let db_edges: Vec<EdgeRowDb> =
        sqlx::query_as::<_, EdgeRowDb>(&query)
            .bind(project_id)
            .fetch_all(ctx.db().pool().expect(
                "inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>",
            ))
            .await
            .map_err(|e| McpError::internal_error(format!("Edge query failed: {}", e), None))?;

    // Build file metas
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

    // Convert to graph builder types
    use crate::graph::algorithms::louvain_communities;
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

    if code_graph.node_count() < 2 {
        return Ok(CallToolResult::success(vec![Content::text(
            "Not enough nodes in the graph for community detection.",
        )]));
    }

    let louvain = louvain_communities(&code_graph.graph, resolution);

    // Build community -> files map
    let mut community_files: std::collections::HashMap<usize, Vec<String>> =
        std::collections::HashMap::new();
    for (&node_idx, &comm) in &louvain.communities {
        if let Some(file_node) = code_graph.graph.node_weight(node_idx) {
            community_files
                .entry(comm)
                .or_default()
                .push(file_node.relative_path.clone());
        }
    }

    // Compare communities with directory structure
    let mut communities: Vec<serde_json::Value> = Vec::new();
    for (comm_id, files) in &community_files {
        // Find dominant directory
        let mut dir_counts: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::new();
        for f in files {
            let dir = f.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
            *dir_counts.entry(dir).or_insert(0) += 1;
        }
        let dominant_dir = dir_counts
            .iter()
            .max_by_key(|(_, c)| *c)
            .map(|(d, _)| *d)
            .unwrap_or("");
        let dir_match_pct =
            dir_counts.get(dominant_dir).copied().unwrap_or(0) as f64 / files.len().max(1) as f64;

        communities.push(serde_json::json!({
            "community_id": comm_id,
            "file_count": files.len(),
            "dominant_directory": dominant_dir,
            "directory_match_pct": format!("{:.1}%", dir_match_pct * 100.0),
            "files": files,
        }));
    }

    communities.sort_by(|a, b| {
        let sa = a["file_count"].as_u64().unwrap_or(0);
        let sb = b["file_count"].as_u64().unwrap_or(0);
        sb.cmp(&sa)
    });

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

    let result = serde_json::json!({
        "effect_breakdown": effect_breakdown,
        "project": params.project,
        "graph_type": graph_type,
        "resolution": resolution,
        "modularity_q": format!("{:.4}", louvain.modularity),
        "community_count": louvain.num_communities,
        "communities": communities,
        "guidance": "Modularity Q > 0.3 indicates strong community structure. \
                     Low directory_match_pct suggests the discovered community differs from \
                     the file system layout — consider reorganizing files to match.",
    });

    let json = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "community_detection",
        communities = louvain.num_communities,
        modularity = %format!("{:.4}", louvain.modularity),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
