//! `tool_change_impact_analysis` — MCP tool body, extracted from `super::super::server`.

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

pub async fn tool_change_impact_analysis(
    ctx: &SystemContext,
    params: ChangeImpactAnalysisParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().impact_scans.fetch_add(1, Ordering::Relaxed);

    let depth = params.depth.unwrap_or(3);
    let include_semantic = params.include_semantic.unwrap_or(true);

    debug!(
        tool = "change_impact_analysis",
        project = %params.project,
        file = %params.file,
        depth,
        include_semantic,
        "MCP tool invoked",
    );

    // Resolve project and file
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

    #[derive(sqlx::FromRow)]
    struct FileId {
        id: i64,
    }

    let target_file: Option<FileId> = sqlx::query_as::<_, FileId>(
        "SELECT id FROM indexed_files WHERE project_id = $1 AND relative_path = $2",
    )
    .bind(project_id)
    .bind(&params.file)
    .fetch_optional(
        ctx.db()
            .pool()
            .expect("inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>"),
    )
    .await
    .map_err(|e| McpError::internal_error(format!("File lookup failed: {}", e), None))?;

    let target_file_id = target_file.map(|f| f.id).ok_or_else(|| {
        McpError::internal_error(format!("File not found: {}", params.file), None)
    })?;

    // 1. Import graph: reverse BFS (files that depend on target)
    #[derive(sqlx::FromRow)]
    #[allow(dead_code)]
    struct DepRow {
        file_id: i64,
        relative_path: String,
        edge_type: String,
    }

    // Files that import this file (direct dependents)
    let import_dependents: Vec<DepRow> = sqlx::query_as::<_, DepRow>(
        "SELECT e.source_file_id as file_id, f.relative_path, e.edge_type
         FROM code_graph_edges e
         JOIN indexed_files f ON e.source_file_id = f.id
         WHERE e.target_file_id = $1 AND e.edge_type = 'import'",
    )
    .bind(target_file_id)
    .fetch_all(
        ctx.db()
            .pool()
            .expect("inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>"),
    )
    .await
    .map_err(|e| McpError::internal_error(format!("Dependents query failed: {}", e), None))?;

    // For deeper impact, do BFS through import edges
    let mut impacted: std::collections::HashMap<i64, (String, f64, String)> =
        std::collections::HashMap::new();
    // (file_id -> (path, impact_score, source_type))

    // Direct import dependents get score 1.0
    let mut frontier: std::collections::VecDeque<(i64, i32)> = std::collections::VecDeque::new();
    for dep in &import_dependents {
        impacted.entry(dep.file_id).or_insert_with(|| {
            frontier.push_back((dep.file_id, 1));
            (dep.relative_path.clone(), 1.0, "import".to_string())
        });
    }

    // BFS for transitive dependents
    while let Some((node, d)) = frontier.pop_front() {
        if d >= depth {
            continue;
        }
        let transitive: Vec<DepRow> =
            sqlx::query_as::<_, DepRow>(
                "SELECT e.source_file_id as file_id, f.relative_path, e.edge_type
             FROM code_graph_edges e
             JOIN indexed_files f ON e.source_file_id = f.id
             WHERE e.target_file_id = $1 AND e.edge_type = 'import'",
            )
            .bind(node)
            .fetch_all(ctx.db().pool().expect(
                "inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>",
            ))
            .await
            .unwrap_or_default();

        for dep in &transitive {
            if dep.file_id == target_file_id {
                continue;
            }
            impacted.entry(dep.file_id).or_insert_with(|| {
                frontier.push_back((dep.file_id, d + 1));
                let decay = 1.0 / (d + 1) as f64;
                (
                    dep.relative_path.clone(),
                    decay,
                    "transitive_import".to_string(),
                )
            });
        }
    }

    // 2. Co-change coupling
    let co_change_pairs = ctx
        .db()
        .find_coupled_files(&params.project, 0.2, 2)
        .await
        .unwrap_or_default();

    for pair in &co_change_pairs {
        let (other_path, other_id_query) = if pair.file_a == params.file {
            (pair.file_b.clone(), pair.file_b.clone())
        } else if pair.file_b == params.file {
            (pair.file_a.clone(), pair.file_a.clone())
        } else {
            continue;
        };

        let other_id: Option<i64> =
            sqlx::query_scalar(
                "SELECT id FROM indexed_files WHERE project_id = $1 AND relative_path = $2",
            )
            .bind(project_id)
            .bind(&other_id_query)
            .fetch_optional(ctx.db().pool().expect(
                "inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>",
            ))
            .await
            .unwrap_or(None);

        if let Some(oid) = other_id {
            impacted.entry(oid).or_insert((
                other_path,
                pair.jaccard * 0.8,
                "co_change".to_string(),
            ));
        }
    }

    // 3. Semantic similarity (optional)
    if include_semantic {
        let similar_files = ctx
            .db()
            // Within-project change-impact: target_project is the same
            // project as the seed file, so the same-repo filter is a
            // no-op. Pass `false` to keep behavior identical.
            .find_similar_files(target_file_id, 0.80, 10, Some(&params.project), false)
            .await
            .unwrap_or_default();

        for sim in &similar_files {
            // Try to resolve the file_id for the similar file
            let sim_id: Option<i64> = sqlx::query_scalar(
                "SELECT id FROM indexed_files WHERE project_id = $1 AND path = $2",
            )
            .bind(project_id)
            .bind(&sim.path_b)
            .fetch_optional(ctx.db().pool().expect(
                "inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>",
            ))
            .await
            .unwrap_or(None);

            if let Some(sid) = sim_id {
                impacted.entry(sid).or_insert((
                    sim.path_b.clone(),
                    sim.avg_similarity * 0.5,
                    "semantic".to_string(),
                ));
            }
        }
    }

    // Build result
    let mut impact_list: Vec<serde_json::Value> = impacted
        .iter()
        .map(|(_id, (path, score, source))| {
            serde_json::json!({
                "path": path,
                "impact_score": format!("{:.4}", score),
                "source": source,
            })
        })
        .collect();

    impact_list.sort_by(|a, b| {
        let sa: f64 = a["impact_score"]
            .as_str()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0.0);
        let sb: f64 = b["impact_score"]
            .as_str()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0.0);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });

    let result = serde_json::json!({
        "project": params.project,
        "target_file": params.file,
        "depth": depth,
        "include_semantic": include_semantic,
        "impacted_file_count": impact_list.len(),
        "impacted_files": impact_list,
        "guidance": "Files with high impact scores are most likely to need changes when the \
                     target file changes. 'import' sources are direct dependents, \
                     'co_change' sources historically change together, \
                     'semantic' sources are functionally related.",
    });

    let json = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "change_impact_analysis",
        impacted = impact_list.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
