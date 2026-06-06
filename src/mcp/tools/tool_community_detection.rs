//! `tool_community_detection` — MCP tool body, extracted from `super::super::server`.

use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::json;
use tracing::debug;

use crate::context::SystemContext;
use crate::mcp::server::*;
use crate::mcp::tools::sota_helpers::{pool_or_err, project_id_or_err};

const DEFAULT_RESOLUTION: f64 = 1.0;
const MIN_RESOLUTION: f64 = 0.05;
const MAX_RESOLUTION: f64 = 10.0;

fn normalize_graph_type(raw: Option<&str>) -> Result<String, McpError> {
    let graph_type = raw.unwrap_or("import").trim().to_ascii_lowercase();
    let graph_type = if graph_type.is_empty() {
        "import".to_string()
    } else {
        graph_type
    };
    if matches!(graph_type.as_str(), "import" | "co_change" | "combined") {
        Ok(graph_type)
    } else {
        Err(McpError::invalid_params(
            "graph_type must be 'import', 'co_change', or 'combined'",
            None,
        ))
    }
}

fn normalize_resolution(raw: Option<f64>) -> Result<f64, McpError> {
    let resolution = raw.unwrap_or(DEFAULT_RESOLUTION);
    if !resolution.is_finite() {
        return Err(McpError::invalid_params("resolution must be finite", None));
    }
    Ok(resolution.clamp(MIN_RESOLUTION, MAX_RESOLUTION))
}

pub async fn tool_community_detection(
    ctx: &SystemContext,
    params: CommunityDetectionParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().community_scans.fetch_add(1, Ordering::Relaxed);

    let project = params.project.trim().to_string();
    let graph_type = normalize_graph_type(params.graph_type.as_deref())?;
    let resolution = normalize_resolution(params.resolution)?;
    let pool = pool_or_err(ctx)?;
    let project_id = project_id_or_err(ctx, &project).await?;

    debug!(
        tool = "community_detection",
        project = %project,
        graph_type = %graph_type,
        resolution,
        "MCP tool invoked",
    );

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
         JOIN indexed_files sf
           ON e.source_file_id = sf.id
          AND sf.project_id = e.project_id
         LEFT JOIN indexed_files tf
           ON e.target_file_id = tf.id
          AND tf.project_id = e.project_id
         WHERE e.project_id = $1
           AND ($2::text = 'combined' OR e.edge_type = $2)
           AND (e.target_file_id IS NULL OR tf.id IS NOT NULL)",
    )
    .bind(project_id)
    .bind(&graph_type)
    .fetch_all(pool)
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
    .fetch_all(pool)
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
            "members": files,
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

    let result = serde_json::json!({
        "effect_breakdown": effect_breakdown,
        "project": project,
        "graph_type": graph_type,
        "resolution": resolution,
        "modularity": louvain.modularity,
        "modularity_q": format!("{:.4}", louvain.modularity),
        "num_communities": louvain.num_communities,
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
