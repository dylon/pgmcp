//! `tool_coupling_cohesion_report` — MCP tool body, extracted from `super::super::server`.

use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use tracing::debug;

use crate::context::SystemContext;
use crate::mcp::server::*;
use crate::mcp::tools::sota_helpers::{pool_or_err, project_id_or_err};

pub async fn tool_coupling_cohesion_report(
    ctx: &SystemContext,
    params: CouplingCohesionReportParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().coupling_reports.fetch_add(1, Ordering::Relaxed);

    let project = params.project.trim().to_string();
    let module_depth = params.module_depth.unwrap_or(2).clamp(1, 8) as usize;
    let sort_by = params
        .sort_by
        .as_deref()
        .map(str::trim)
        .filter(|sort_by| !sort_by.is_empty())
        .unwrap_or("distance");
    if !matches!(
        sort_by,
        "instability" | "distance" | "coupling" | "cohesion"
    ) {
        return Err(McpError::invalid_params(
            format!(
                "Unknown sort_by '{}': expected one of instability | distance | coupling | cohesion",
                sort_by
            ),
            None,
        ));
    }

    debug!(
        tool = "coupling_cohesion_report",
        project = %project,
        module_depth,
        sort_by,
        "MCP tool invoked",
    );

    let pool = pool_or_err(ctx)?;
    let project_id = project_id_or_err(ctx, &project).await?;

    // Load edges and files, build graph
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
         JOIN indexed_files sf ON e.source_file_id = sf.id AND sf.project_id = e.project_id
         LEFT JOIN indexed_files tf ON e.target_file_id = tf.id AND tf.project_id = e.project_id
         WHERE e.project_id = $1
           AND e.edge_type = 'import'
           AND (e.target_file_id IS NULL OR tf.id IS NOT NULL)",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Edge query failed: {}", e), None))?;

    #[derive(sqlx::FromRow)]
    struct FileMetaDb {
        file_id: i64,
        relative_path: String,
        language: String,
        content: Option<String>,
    }

    let file_data: Vec<FileMetaDb> = sqlx::query_as::<_, FileMetaDb>(
        "SELECT id as file_id, relative_path, language, content
         FROM indexed_files WHERE project_id = $1",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("File query failed: {}", e), None))?;

    use crate::graph::builder::{FileMetaRow, GraphEdgeRow, build_graph};
    use crate::graph::metrics::{compute_module_metrics, is_abstract_file, update_abstractness};

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

    let metas: Vec<FileMetaRow> = file_data
        .iter()
        .map(|f| FileMetaRow {
            file_id: f.file_id,
            relative_path: f.relative_path.clone(),
            language: f.language.clone(),
        })
        .collect();

    let code_graph = build_graph(&graph_edges, &metas);
    let mut module_metrics = compute_module_metrics(&code_graph, module_depth);

    // Compute abstractness from content
    let mut file_abstractions: std::collections::HashMap<String, bool> =
        std::collections::HashMap::new();
    for f in &file_data {
        let is_abstract = f
            .content
            .as_ref()
            .map(|c| is_abstract_file(c, &f.language))
            .unwrap_or(false);
        file_abstractions.insert(f.relative_path.clone(), is_abstract);
    }
    update_abstractness(&mut module_metrics, &file_abstractions);

    // Sort
    match sort_by {
        "instability" => module_metrics.sort_by(|a, b| {
            b.instability
                .partial_cmp(&a.instability)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        "coupling" => module_metrics.sort_by(|a, b| {
            let ca = a.afferent_coupling + a.efferent_coupling;
            let cb = b.afferent_coupling + b.efferent_coupling;
            cb.cmp(&ca)
        }),
        "cohesion" => module_metrics.sort_by(|a, b| {
            let ca = a.cohesion.unwrap_or(0.0);
            let cb = b.cohesion.unwrap_or(0.0);
            ca.partial_cmp(&cb).unwrap_or(std::cmp::Ordering::Equal)
        }),
        _ => module_metrics.sort_by(|a, b| {
            b.distance_from_main_sequence
                .partial_cmp(&a.distance_from_main_sequence)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
    }

    const MAX_REPORTED_MODULES: usize = 2000;
    let total_module_count = module_metrics.len();
    let truncated = total_module_count > MAX_REPORTED_MODULES;

    let modules: Vec<serde_json::Value> = module_metrics
        .iter()
        .take(MAX_REPORTED_MODULES)
        .map(|m| {
            let zone = if m.instability < 0.3 && m.abstractness < 0.3 {
                "zone_of_pain"
            } else if m.instability > 0.7 && m.abstractness > 0.7 {
                "zone_of_uselessness"
            } else if m.distance_from_main_sequence < 0.3 {
                "main_sequence"
            } else {
                "acceptable"
            };
            serde_json::json!({
                "module": m.module_path,
                "file_count": m.file_count,
                "afferent_coupling": m.afferent_coupling,
                "efferent_coupling": m.efferent_coupling,
                "instability": format!("{:.4}", m.instability),
                "abstractness": format!("{:.4}", m.abstractness),
                "distance": format!("{:.4}", m.distance_from_main_sequence),
                "zone": zone,
            })
        })
        .collect();

    // Shadow-ASR channel (Phase D2b): per-effect symbol-count breakdown
    // for the project. Universal enrichment — every tool benefits from
    // surfacing the effect distribution alongside its primary output.
    // Gracefully degrades to empty when the project lookup or
    // shadow-ASR data isn't populated.
    let effect_breakdown: Vec<serde_json::Value> =
        crate::mcp::tools::sema_helpers::effects::effect_counts(pool, project_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(eff, count)| serde_json::json!({ "effect": eff, "count": count }))
            .collect();

    let result = serde_json::json!({
        "effect_breakdown": effect_breakdown,
        "project": project,
        "module_depth": module_depth,
        "sort_by": sort_by,
        "module_count": modules.len(),
        "total_module_count": total_module_count,
        "truncated": truncated,
        "modules": modules,
        "guidance": "D* close to 0 = on the Main Sequence (ideal balance of A+I). \
                     Zone of Pain (low A, low I): concrete and stable — hard to change. \
                     Zone of Uselessness (high A, high I): abstract and unstable — over-engineered.",
    });

    let json = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "coupling_cohesion_report",
        modules = modules.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
