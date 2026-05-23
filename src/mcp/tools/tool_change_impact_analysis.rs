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

    // Shadow-ASR Pattern C: add symbol-level reverse-reachability via
    // resolved call edges. For each symbol declared in the target file,
    // walk the reverse-edge subgraph (callers → callers-of-callers …)
    // for `depth` hops. Files containing any reached symbol are added
    // as additional impacted files with source "resolved_caller".
    {
        type SymIdRow = (i64,);
        let target_syms: Vec<SymIdRow> =
            sqlx::query_as("SELECT id FROM file_symbols WHERE file_id = $1")
                .bind(target_file_id)
                .fetch_all(ctx.db().pool().expect(
                    "inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>",
                ))
                .await
                .unwrap_or_default();
        let seed_ids: Vec<i64> = target_syms.iter().map(|(id,)| *id).collect();
        if !seed_ids.is_empty() {
            // BFS over reversed resolved edges.
            use std::collections::{HashSet, VecDeque};
            let mut visited: HashSet<i64> = seed_ids.iter().copied().collect();
            let mut frontier: VecDeque<(i64, u32)> =
                seed_ids.iter().map(|&id| (id, 0u32)).collect();
            let max_depth = depth as u32;
            while let Some((sid, d)) = frontier.pop_front() {
                if d >= max_depth {
                    continue;
                }
                let callers: Vec<i64> = sqlx::query_scalar(
                    "SELECT DISTINCT sr.source_symbol_id
                     FROM symbol_references sr
                     WHERE sr.target_symbol_id = $1
                       AND sr.source_symbol_id IS NOT NULL
                       AND sr.resolution_kind IN ('exact_in_file', 'exact_via_import')",
                )
                .bind(sid)
                .fetch_all(ctx.db().pool().expect(
                    "inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>",
                ))
                .await
                .unwrap_or_default();
                for c in callers {
                    if visited.insert(c) {
                        frontier.push_back((c, d + 1));
                    }
                }
            }
            // Resolve visited symbol ids to (file_id, path).
            if !visited.is_empty() {
                let visited_vec: Vec<i64> = visited.into_iter().collect();
                type FileRow = (i64, String);
                let reached_files: Vec<FileRow> = sqlx::query_as(
                    "SELECT DISTINCT fs.file_id, f.relative_path
                     FROM file_symbols fs
                     JOIN indexed_files f ON f.id = fs.file_id
                     WHERE fs.id = ANY($1::int8[])",
                )
                .bind(&visited_vec)
                .fetch_all(ctx.db().pool().expect(
                    "inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>",
                ))
                .await
                .unwrap_or_default();
                for (fid, path) in reached_files {
                    if fid == target_file_id {
                        continue;
                    }
                    impacted
                        .entry(fid)
                        .or_insert((path, 0.75, "resolved_caller".to_string()));
                }
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
