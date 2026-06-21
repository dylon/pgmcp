//! `tool_architecture_violations` — MCP tool body, extracted from `super::super::server`.

use std::sync::atomic::Ordering;
use std::time::Instant;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use tracing::debug;

use crate::context::SystemContext;
use crate::mcp::server::*;
use crate::mcp::tools::sota_helpers::{pool_or_err, project_id_or_err};

pub async fn tool_architecture_violations(
    ctx: &SystemContext,
    params: ArchitectureViolationsParams,
) -> Result<CallToolResult, McpError> {
    let start = Instant::now();
    ctx.stats().mcp_requests.fetch_add(1, Ordering::Relaxed);
    ctx.stats().violation_scans.fetch_add(1, Ordering::Relaxed);

    let project = params.project.trim().to_string();
    let severity_threshold = params
        .severity_threshold
        .as_deref()
        .map(str::trim)
        .filter(|severity| !severity.is_empty())
        .unwrap_or("medium");
    if !matches!(severity_threshold, "low" | "medium" | "high" | "critical") {
        return Err(McpError::invalid_params(
            format!(
                "Unknown severity_threshold '{}': expected one of low | medium | high | critical",
                severity_threshold
            ),
            None,
        ));
    }
    let include_fixes = params.include_fixes.unwrap_or(true);

    // Default exclusions for intentional organization patterns that look
    // like god modules to a per-directory file-count rule. Override per-
    // call via the `excluded_god_module_prefixes` parameter; the empty
    // vector form `Some(vec![])` disables exclusions entirely so the raw
    // file-count rule applies.
    // `src/mcp` (290 files) is the depth-2 rollup of the deliberate
    // one-file-per-tool idiom under `src/mcp/tools`; the depth-3
    // `src/mcp/tools` prefix does not match the `src/mcp` module key, so the
    // aggregate must be excluded explicitly or it dominates the report.
    const DEFAULT_GOD_MODULE_EXCLUSIONS: &[&str] = &[
        "src/patterns",
        "src/mcp",
        "src/mcp/tools",
        "pgmcp-testing/tests",
    ];
    let god_module_exclusions: Vec<String> = params
        .excluded_god_module_prefixes
        .clone()
        .unwrap_or_else(|| {
            DEFAULT_GOD_MODULE_EXCLUSIONS
                .iter()
                .map(|s| (*s).to_string())
                .collect()
        });
    let god_module_exclusions: Vec<String> = god_module_exclusions
        .into_iter()
        .map(|prefix| prefix.trim().trim_end_matches('/').to_string())
        .filter(|prefix| !prefix.is_empty())
        .collect();

    debug!(
        tool = "architecture_violations",
        project = %project,
        severity_threshold,
        include_fixes,
        "MCP tool invoked",
    );

    let mut violations: Vec<serde_json::Value> = Vec::new();
    let pool = pool_or_err(ctx)?;

    // 1. Check for dependency cycles (critical)
    let project_id = project_id_or_err(ctx, &project).await?;

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
    }

    let file_metas: Vec<FileMetaDb> = sqlx::query_as::<_, FileMetaDb>(
        "SELECT id as file_id, relative_path, language FROM indexed_files WHERE project_id = $1",
    )
    .bind(project_id)
    .fetch_all(pool)
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
            // Skip intentionally-large directory patterns (one-file-per-tool
            // catalogs, per-family pattern files, integration-test suites).
            // Match on prefix to catch both top-level and nested forms.
            if god_module_exclusions
                .iter()
                .any(|p| module == p || module.starts_with(&format!("{}/", p)))
            {
                continue;
            }
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

    // 5. Zone of Pain / Zone of Uselessness — abstractness from the persisted,
    // symbol-derived per-file metric (single source of truth) rather than a
    // file-name heuristic.
    let file_abstractions: std::collections::HashMap<String, bool> =
        crate::db::queries::file_abstractions(pool, project_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .collect();

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

    // Reflexion-model conformance (graph-roadmap Phase 3.2): when the project
    // declares layer rules in `.pgmcp.toml [architecture]`, map each file to a
    // layer by path prefix and flag import edges that violate the declared
    // `allow` rules (divergences). No declared rules ⇒ skipped (purely additive).
    let mut reflexion_json = serde_json::Value::Null;
    {
        let root: Option<String> = sqlx::query_scalar("SELECT path FROM projects WHERE id = $1")
            .bind(project_id)
            .fetch_optional(pool)
            .await
            .unwrap_or(None);
        if let Some(root) = root
            && let Some(po) = crate::config::ProjectOverride::load(std::path::Path::new(&root))
            && let Some(arch) = po.architecture
        {
            let model = crate::code_analysis::reflexion::LayerModel::from_rules(&arch);
            if !model.is_empty() {
                let summary = model.summarize(db_edges.iter().filter_map(|e| {
                    e.target_relative_path
                        .as_deref()
                        .map(|t| (e.source_relative_path.as_str(), t))
                }));
                // Push divergences as layer_violation entries (capped so a badly
                // mis-layered project can't flood the response).
                const MAX_LAYER_VIOLATIONS: usize = 100;
                for d in summary.divergences.iter().take(MAX_LAYER_VIOLATIONS) {
                    violations.push(serde_json::json!({
                        "type": "layer_violation",
                        "severity": "high",
                        "description": format!(
                            "Illegal dependency: layer '{}' must not import layer '{}'",
                            d.from_layer, d.to_layer
                        ),
                        "from_layer": d.from_layer,
                        "to_layer": d.to_layer,
                        "source_file": d.src_path,
                        "target_file": d.dst_path,
                    }));
                }
                let absences = model.absences(&summary.realized_pairs);
                reflexion_json = serde_json::json!({
                    "convergences": summary.convergences,
                    "divergence_count": summary.divergences.len(),
                    "divergences_reported": summary.divergences.len().min(MAX_LAYER_VIOLATIONS),
                    "unlayered_edges": summary.unlayered,
                    "absences": absences
                        .iter()
                        .map(|(f, t)| serde_json::json!({ "from": f, "to": t }))
                        .collect::<Vec<_>>(),
                });
            }
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

    const MAX_REPORTED_VIOLATIONS: usize = 500;
    let total_violation_count = violations.len();
    let truncated = total_violation_count > MAX_REPORTED_VIOLATIONS;
    violations.truncate(MAX_REPORTED_VIOLATIONS);

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
            if let Some(fix) = default_fix_for_violation(&vtype, &project, &files, module)
                && let Ok(fix_json) = serde_json::to_value(&fix)
                && let Some(obj) = v.as_object_mut()
            {
                obj.insert("recommended_fix".to_string(), fix_json);
            }
        }
    }

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
        "severity_threshold": severity_threshold,
        "violation_count": violations.len(),
        "total_violation_count": total_violation_count,
        "truncated": truncated,
        "violations": violations,
        "reflexion": reflexion_json,
        "guidance": "Fix critical violations first (cycles), then high (god modules, bidirectional deps, \
                     layer violations), then medium (SDP violations, Zone of Pain). Each violation includes \
                     specific files/modules. `reflexion` is populated only when the project declares \
                     `[architecture]` layers + allow-rules in .pgmcp.toml: divergences are illegal cross-layer \
                     imports (also listed as `layer_violation` entries), absences are declared-but-unused \
                     dependencies.",
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
