//! `tool_architecture_violations` — MCP tool body, extracted from `super::super::server`.

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

pub async fn tool_architecture_violations(
    ctx: &SystemContext,
    params: ArchitectureViolationsParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().violation_scans.fetch_add(1, Ordering::Relaxed);

    let severity_threshold = params.severity_threshold.as_deref().unwrap_or("medium");
    let include_fixes = params.include_fixes.unwrap_or(true);

    info!(
        tool = "architecture_violations",
        project = %params.project,
        severity_threshold,
        include_fixes,
        "MCP tool invoked",
    );

    let mut violations: Vec<serde_json::Value> = Vec::new();

    // 1. Check for dependency cycles (critical)
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

    // Load import edges and build graph for cycle detection
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
        "SELECT id as file_id, relative_path, language FROM indexed_files WHERE project_id = $1",
    )
    .bind(project_id)
    .fetch_all(
        ctx.db()
            .pool()
            .expect("inline SQL needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient>"),
    )
    .await
    .map_err(|e| McpError::internal_error(format!("File query failed: {}", e), None))?;

    use crate::graph::algorithms::find_cycles;
    use crate::graph::builder::{FileMetaRow, GraphEdgeRow, build_graph};
    use crate::graph::metrics::{compute_module_metrics, update_abstractness};

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

    // Dependency cycles
    let sccs = find_cycles(&code_graph.graph);
    for scc in &sccs {
        let files: Vec<&str> = scc
            .iter()
            .filter_map(|n| {
                code_graph
                    .graph
                    .node_weight(*n)
                    .map(|f| f.relative_path.as_str())
            })
            .collect();
        violations.push(serde_json::json!({
            "type": "dependency_cycle",
            "severity": "critical",
            "description": format!("Circular dependency among {} files", files.len()),
            "files": files,
        }));
    }

    // 2. God modules (>15 files)
    let mut module_files: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for node_idx in code_graph.graph.node_indices() {
        let node = &code_graph.graph[node_idx];
        let module = node.module.split('/').take(2).collect::<Vec<_>>().join("/");
        module_files
            .entry(module)
            .or_default()
            .push(node.relative_path.clone());
    }
    for (module, files) in &module_files {
        if files.len() > 15 {
            violations.push(serde_json::json!({
                "type": "god_module",
                "severity": "high",
                "description": format!("Module '{}' has {} files (threshold: 15)", module, files.len()),
                "module": module,
                "file_count": files.len(),
            }));
        }
    }

    // 3. Bidirectional dependencies
    let mut edge_pairs: std::collections::HashSet<(i64, i64)> = std::collections::HashSet::new();
    for e in &db_edges {
        if let Some(tid) = e.target_file_id {
            if edge_pairs.contains(&(tid, e.source_file_id)) {
                violations.push(serde_json::json!({
                    "type": "bidirectional_dependency",
                    "severity": "high",
                    "description": format!("{} <-> {}", e.source_relative_path,
                        e.target_relative_path.as_deref().unwrap_or("?")),
                    "file_a": e.source_relative_path,
                    "file_b": e.target_relative_path,
                }));
            }
            edge_pairs.insert((e.source_file_id, tid));
        }
    }

    // 4. SDP violations: unstable module depends on more unstable module
    let module_metrics = compute_module_metrics(&code_graph, 2);
    let module_instability: std::collections::HashMap<&str, f64> = module_metrics
        .iter()
        .map(|m| (m.module_path.as_str(), m.instability))
        .collect();

    for e in &db_edges {
        if let Some(ref target_path) = e.target_relative_path {
            let source_module = e
                .source_relative_path
                .rsplit_once('/')
                .map(|(d, _)| d)
                .unwrap_or("");
            let target_module = target_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
            if source_module != target_module {
                let source_i = module_instability
                    .get(source_module)
                    .copied()
                    .unwrap_or(0.5);
                let target_i = module_instability
                    .get(target_module)
                    .copied()
                    .unwrap_or(0.5);
                // SDP: stable modules should not depend on unstable modules
                if source_i < 0.3 && target_i > 0.7 {
                    violations.push(serde_json::json!({
                        "type": "sdp_violation",
                        "severity": "medium",
                        "description": format!("Stable module '{}' (I={:.2}) depends on unstable '{}' (I={:.2})",
                            source_module, source_i, target_module, target_i),
                        "source_module": source_module,
                        "target_module": target_module,
                        "source_instability": format!("{:.2}", source_i),
                        "target_instability": format!("{:.2}", target_i),
                    }));
                }
            }
        }
    }

    // 5. Zone of Pain / Zone of Uselessness
    // Need abstractness — load file content for a quick check
    let mut file_abstractions: std::collections::HashMap<String, bool> =
        std::collections::HashMap::new();
    for f in &file_metas {
        // Quick heuristic: check file name patterns
        let is_abstract = f.relative_path.contains("trait")
            || f.relative_path.contains("interface")
            || f.relative_path.contains("abstract")
            || f.relative_path.ends_with("mod.rs");
        file_abstractions.insert(f.relative_path.clone(), is_abstract);
    }

    let mut mm = module_metrics;
    update_abstractness(&mut mm, &file_abstractions);

    for m in &mm {
        if m.instability < 0.3 && m.abstractness < 0.3 && m.file_count > 3 {
            violations.push(serde_json::json!({
                "type": "zone_of_pain",
                "severity": "medium",
                "description": format!("Module '{}' is in Zone of Pain (I={:.2}, A={:.2})",
                    m.module_path, m.instability, m.abstractness),
                "module": m.module_path,
            }));
        }
        if m.instability > 0.7 && m.abstractness > 0.7 && m.file_count > 2 {
            violations.push(serde_json::json!({
                "type": "zone_of_uselessness",
                "severity": "low",
                "description": format!("Module '{}' is in Zone of Uselessness (I={:.2}, A={:.2})",
                    m.module_path, m.instability, m.abstractness),
                "module": m.module_path,
            }));
        }
    }

    // Filter by severity threshold
    let severity_order = |s: &str| -> i32 {
        match s {
            "critical" => 4,
            "high" => 3,
            "medium" => 2,
            "low" => 1,
            _ => 0,
        }
    };
    let threshold = severity_order(severity_threshold);
    violations.retain(|v| severity_order(v["severity"].as_str().unwrap_or("low")) >= threshold);

    violations.sort_by(|a, b| {
        let sa = severity_order(a["severity"].as_str().unwrap_or("low"));
        let sb = severity_order(b["severity"].as_str().unwrap_or("low"));
        sb.cmp(&sa)
    });

    // Phase 1 backfill: attach a typed `recommended_fix` to each violation.
    // Off when include_fixes=false, which reproduces today's diagnostic-only shape.
    if include_fixes {
        use crate::mcp::tools::fix_helpers::default_fix_for_violation;
        for v in &mut violations {
            let vtype = match v["type"].as_str() {
                Some(t) => t.to_string(),
                None => continue,
            };
            // Collect file references from whichever fields the violation type populates.
            let files: Vec<String> = match vtype.as_str() {
                "dependency_cycle" => v["files"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default(),
                "bidirectional_dependency" => {
                    let mut out = Vec::new();
                    if let Some(a) = v["file_a"].as_str() {
                        out.push(a.to_string());
                    }
                    if let Some(b) = v["file_b"].as_str() {
                        out.push(b.to_string());
                    }
                    out
                }
                _ => Vec::new(),
            };
            let module = v["module"]
                .as_str()
                .or_else(|| v["source_module"].as_str())
                .or_else(|| v["target_module"].as_str());
            if let Some(fix) = default_fix_for_violation(&vtype, &params.project, &files, module)
                && let Ok(fix_json) = serde_json::to_value(&fix)
                && let Some(obj) = v.as_object_mut()
            {
                obj.insert("recommended_fix".to_string(), fix_json);
            }
        }
    }

    let result = serde_json::json!({
        "project": params.project,
        "severity_threshold": severity_threshold,
        "violation_count": violations.len(),
        "violations": violations,
        "guidance": "Fix critical violations first (cycles), then high (god modules, bidirectional deps), \
                     then medium (SDP violations, Zone of Pain). Each violation includes specific files/modules.",
    });

    let json = serde_json::to_string_pretty(&result)
        .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

    debug!(
        tool = "architecture_violations",
        violations = violations.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "MCP tool completed",
    );

    Ok(CallToolResult::success(vec![Content::text(json)]))
}
