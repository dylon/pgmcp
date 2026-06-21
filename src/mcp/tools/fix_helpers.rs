//! Shared helpers for recommendation-shaped tools.
//!
//! These pull duplicated logic out of `tool_architecture_violations.rs` and
//! `tool_circular_dependencies.rs` (which both embed identical inline SQL +
//! graph-row mapping). New recommendation tools (Tier 2-5) reuse these so
//! callsite/import detection stays consistent across the catalogue.
//!
//! The graph-loading path returns the full `(CodeGraph, edge rows, file-meta
//! rows)` tuple so callers that need the raw rows (e.g. for bidirectional-edge
//! detection) don't have to re-query.
//!
//! `find_function_callers` is regex-based at first; once Tier 0e (tree-sitter
//! parsing layer) lands, this helper switches to `symbol_references` lookups
//! and the confidence reported by callers naturally rises.

#![allow(dead_code)] // helpers are referenced from new tool bodies (Tier 2-5)
// that aren't yet wired into the #[tool_router] dispatch.

use std::collections::HashMap;
use std::sync::OnceLock;

use regex::Regex;
use rmcp::ErrorData as McpError;
use sqlx::PgPool;

use crate::context::SystemContext;
use crate::graph::CodeGraph;
use crate::graph::builder::{FileMetaRow, GraphEdgeRow, build_graph};
use crate::mcp::tools::fix_actions::{
    EstimatedEffort, FileLine, FixAction, LocationRef, PathRange, RecommendedFix, TargetPath,
    TargetRef,
};

/// Helper: extract the underlying `PgPool` from `SystemContext` for inline SQL.
/// Mirrors the `expect("...")` idiom already used in `tool_architecture_violations.rs:42`
/// — once a query lands in `src/db/queries.rs` it goes through `Arc<dyn DbClient>`.
pub fn pool_or_err(ctx: &SystemContext) -> Result<&PgPool, McpError> {
    ctx.db().pool().ok_or_else(|| {
        McpError::internal_error(
            "DbClient is not backed by a PgPool — fix_helpers requires inline SQL access",
            None,
        )
    })
}

/// Database-shape edge row, mirrors the inline struct in the two existing tools.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct EdgeRowDb {
    pub source_file_id: i64,
    pub source_relative_path: String,
    pub source_language: String,
    pub target_file_id: Option<i64>,
    pub target_relative_path: Option<String>,
    pub target_language: Option<String>,
    pub edge_type: String,
    pub weight: f64,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FileMetaDb {
    pub file_id: i64,
    pub relative_path: String,
    pub language: String,
}

/// Bundle returned by [`load_import_graph`] — the in-memory graph plus the raw
/// rows that built it. Callers that need bidirectional-edge detection or
/// per-edge attributes use `edges`; the rest can ignore them.
pub struct ImportGraphBundle {
    pub graph: CodeGraph,
    pub edges: Vec<EdgeRowDb>,
    pub file_metas: Vec<FileMetaDb>,
}

/// Resolve a project name to its `id`. Returns `None` if no such project exists.
pub async fn lookup_project_id(
    ctx: &SystemContext,
    project_name: &str,
) -> Result<Option<i32>, McpError> {
    let pool = pool_or_err(ctx)?;
    sqlx::query_scalar::<_, i32>("SELECT id FROM projects WHERE name = $1")
        .bind(project_name)
        .fetch_optional(pool)
        .await
        .map_err(|e| McpError::internal_error(format!("Project lookup failed: {}", e), None))
}

/// Load the project's import-edge subgraph + file metadata, returning the
/// composed `CodeGraph` plus the raw rows. Equivalent to the inline queries
/// at `tool_architecture_violations.rs:38-134` and
/// `tool_circular_dependencies.rs:36-132` — extracting here is a pure refactor.
pub async fn load_import_graph(
    ctx: &SystemContext,
    project_id: i32,
) -> Result<ImportGraphBundle, McpError> {
    let pool = pool_or_err(ctx)?;

    let edges: Vec<EdgeRowDb> = sqlx::query_as::<_, EdgeRowDb>(
        "SELECT
            e.source_file_id,
            sf.relative_path AS source_relative_path,
            sf.language AS source_language,
            e.target_file_id,
            tf.relative_path AS target_relative_path,
            tf.language AS target_language,
            e.edge_type,
            e.weight
         FROM code_graph_edges e
         JOIN indexed_files sf ON e.source_file_id = sf.id
         LEFT JOIN indexed_files tf ON e.target_file_id = tf.id
         WHERE e.project_id = $1 AND e.edge_type = 'import'
           AND e.target_project_id IS NULL",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Edge query failed: {}", e), None))?;

    let file_metas: Vec<FileMetaDb> = sqlx::query_as::<_, FileMetaDb>(
        "SELECT id AS file_id, relative_path, language
         FROM indexed_files WHERE project_id = $1",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("File query failed: {}", e), None))?;

    let graph_edges: Vec<GraphEdgeRow> = edges
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

    let graph = build_graph(&graph_edges, &metas);

    Ok(ImportGraphBundle {
        graph,
        edges,
        file_metas,
    })
}

/// Load the project's **full** dependency graph — every `edge_type`
/// (`import` / `call` / `co_change` / `semantic`) at file granularity — plus
/// file metadata, returning the composed `CodeGraph`. Identical to
/// `load_import_graph` minus the `edge_type = 'import'` filter; used by
/// graph-aware retrieval (code-PPR, Phase 3.3) where all relatedness signals
/// help expansion, not just structural imports.
pub async fn load_code_graph_all_edges(
    ctx: &SystemContext,
    project_id: i32,
) -> Result<ImportGraphBundle, McpError> {
    let pool = pool_or_err(ctx)?;

    let edges: Vec<EdgeRowDb> = sqlx::query_as::<_, EdgeRowDb>(
        "SELECT
            e.source_file_id,
            sf.relative_path AS source_relative_path,
            sf.language AS source_language,
            e.target_file_id,
            tf.relative_path AS target_relative_path,
            tf.language AS target_language,
            e.edge_type,
            e.weight
         FROM code_graph_edges e
         JOIN indexed_files sf ON e.source_file_id = sf.id
         LEFT JOIN indexed_files tf ON e.target_file_id = tf.id
         WHERE e.project_id = $1 AND e.target_file_id IS NOT NULL
           AND e.target_project_id IS NULL",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("Edge query failed: {}", e), None))?;

    let file_metas: Vec<FileMetaDb> = sqlx::query_as::<_, FileMetaDb>(
        "SELECT id AS file_id, relative_path, language
         FROM indexed_files WHERE project_id = $1",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(|e| McpError::internal_error(format!("File query failed: {}", e), None))?;

    let graph_edges: Vec<GraphEdgeRow> = edges
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

    let graph = build_graph(&graph_edges, &metas);

    Ok(ImportGraphBundle {
        graph,
        edges,
        file_metas,
    })
}

/// List the files that import (have an outgoing edge into) `target_file_id`,
/// using only the in-memory graph (no DB roundtrip). Returns `(file_id, path)`
/// pairs sorted by path for deterministic output.
pub fn count_importers(graph: &CodeGraph, target_file_id: i64) -> Vec<(i64, String)> {
    use petgraph::Direction;

    let target_idx = match graph.file_id_to_node.get(&target_file_id) {
        Some(idx) => *idx,
        None => return Vec::new(),
    };

    let mut out: Vec<(i64, String)> = graph
        .graph
        .neighbors_directed(target_idx, Direction::Incoming)
        .filter_map(|n| {
            graph
                .graph
                .node_weight(n)
                .map(|file_node| (file_node.file_id, file_node.relative_path.clone()))
        })
        .collect();

    out.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    out
}

/// Lightweight fallback callsite scan: regex-based over a single file's content.
///
/// Matches `<word-boundary><fn_name>(`. Returns line numbers (1-indexed)
/// where the pattern occurs. Used when `symbol_references` data is absent;
/// once Tier 0e is warm, the preferred path is the symbol-based caller
/// lookup that lives in `symbol_references` queries (e.g. through the
/// `trigger_cron` MCP tool's `call-graph` subcommand). The regex
/// fallback remains as the always-available bootstrap path when the
/// symbol graph hasn't been populated yet.
pub fn scan_callsites_regex(content: &str, fn_name: &str) -> Vec<u32> {
    if fn_name.is_empty() {
        return Vec::new();
    }
    // Cache compiled regexes — function-name patterns repeat often during a tool run.
    static CACHE: OnceLock<std::sync::Mutex<HashMap<String, Regex>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let pattern = format!(r"\b{}\s*\(", regex::escape(fn_name));
    let mut guard = match cache.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let re = guard
        .entry(pattern.clone())
        .or_insert_with(|| Regex::new(&pattern).expect("hand-built fn-call regex is always valid"));

    let mut hits = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        if re.is_match(line) {
            hits.push((idx as u32) + 1);
        }
    }
    hits
}

/// Per-file callsite hits, bundled as `FileLine` for direct embedding in
/// `RecommendedFix.references`.
pub fn callsites_to_file_lines(path: &str, lines: &[u32]) -> Vec<FileLine> {
    lines
        .iter()
        .map(|&line| FileLine {
            path: path.to_string(),
            line,
        })
        .collect()
}

/// Convert a topic's c-TF-IDF keyword list into a snake_case function name.
///
/// Picks the top two non-empty tokens, lowercases, strips non-`[a-z0-9_]`
/// characters, joins with `_`. If the keyword list is empty, returns
/// `"shared_helper"` as a stable fallback.
pub fn propose_function_name(keywords: &[String]) -> String {
    let cleaned: Vec<String> = keywords
        .iter()
        .filter_map(|k| {
            let s: String = k
                .to_ascii_lowercase()
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if s.is_empty() { None } else { Some(s) }
        })
        .take(2)
        .collect();
    if cleaned.is_empty() {
        "shared_helper".to_string()
    } else {
        cleaned.join("_")
    }
}

/// Convert a topic's c-TF-IDF keyword list into a kebab-cased crate/module
/// name (e.g. `validation-utils` for `["validate", "schema"]`).
///
/// Returns `"shared-utils"` as a stable fallback for empty keyword lists.
pub fn infer_module_name_from_topics(keywords: &[String]) -> String {
    let cleaned: Vec<String> = keywords
        .iter()
        .filter_map(|k| {
            let s: String = k
                .to_ascii_lowercase()
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                .collect();
            if s.is_empty() { None } else { Some(s) }
        })
        .take(2)
        .collect();
    if cleaned.is_empty() {
        "shared-utils".to_string()
    } else {
        format!("{}-utils", cleaned.join("-"))
    }
}

// ============================================================================
// Default RecommendedFix builders for the Tier-1 enhancement (Phase 1).
//
// These produce a useful, contract-compliant `RecommendedFix` for each
// architecture violation / design smell type. They are intentionally cautious:
// the steps describe the action plainly and point at the dedicated Phase-3 tool
// that will refine into concrete file:line moves once it lands. Confidence is
// moderate (0.45-0.65) — full delegation to `recommend_module_split` /
// `fix_circular_dependency` / `shotgun_surgery_fix` / `stale_zombie_detector`
// in Phase 3 will lift it.
//
// One builder per violation/smell type. The match in
// `default_fix_for_violation` / `default_fix_for_smell` is the central
// dispatch — adding a new type requires a new arm only.
// ============================================================================

/// Build a `RecommendedFix` for an architecture-violation finding. Returns `None` for
/// violation types we don't know how to fix (defensive: the type list is fixed,
/// but if a new variant is added without a builder the tool stays correct).
pub fn default_fix_for_violation(
    violation_type: &str,
    project: &str,
    files: &[String],
    module: Option<&str>,
) -> Option<RecommendedFix> {
    match violation_type {
        "dependency_cycle" => {
            // Pick the highest-indexed file as the "where to introduce a port" hint;
            // Phase 3's fix_circular_dependency replaces this with a real
            // PageRank-delta-minimizing edge selection.
            let hint = files.last().cloned().unwrap_or_else(|| "?".to_string());
            Some(
                RecommendedFix::new(FixAction::ExtractInterface, project)
                    .with_confidence(0.50)
                    .with_effort(EstimatedEffort::Medium)
                    .add_step(format!(
                        "Cycle spans {} files: {}. Break it by extracting a trait/interface \
                         on the side with higher abstractness — invoke `fix_circular_dependency` \
                         for a specific edge to break.",
                        files.len(),
                        files.join(", ")
                    ))
                    .add_target(TargetPath {
                        suggested_new_path: Some(format!("{}_port.rs", trim_extension(&hint))),
                        ..Default::default()
                    }),
            )
        }
        "god_module" => {
            let module_name = module.unwrap_or("?");
            // NOTE: callers pass the file list via the violation's `description`
            // (which already carries the real count), not via `files` here —
            // `files` is empty for god_module. Earlier code interpolated
            // `files.len()` and so always printed "has 0 files", contradicting
            // the description. State the rule, not a bogus count.
            Some(
                RecommendedFix::new(FixAction::SplitFile, project)
                    .with_confidence(0.45)
                    .with_effort(EstimatedEffort::Large)
                    .add_step(format!(
                        "Module '{}' exceeds the per-directory file-count threshold; \
                         consider extracting cohesive sub-modules. Invoke \
                         `recommend_module_split` (Phase 3) for a chunk-cluster→file mapping.",
                        module_name
                    )),
            )
        }
        "bidirectional_dependency" => {
            let (file_a, file_b) = match files {
                [a, b, ..] => (a.as_str(), b.as_str()),
                [a] => (a.as_str(), "?"),
                _ => return None,
            };
            Some(
                RecommendedFix::new(FixAction::InvertDependency, project)
                    .with_confidence(0.60)
                    .with_effort(EstimatedEffort::Medium)
                    .add_step(format!(
                        "Files {} and {} import each other. Identify the lower-PageRank side \
                         (less central) and convert its dependency on the other into a trait/interface, \
                         then `impl` the trait at the higher-central site.",
                        file_a, file_b
                    )),
            )
        }
        "sdp_violation" => {
            let module_name = module.unwrap_or("?");
            Some(
                RecommendedFix::new(FixAction::ExtractInterface, project)
                    .with_confidence(0.55)
                    .with_effort(EstimatedEffort::Medium)
                    .add_step(format!(
                        "Stable module depends on unstable target '{}'. Introduce a trait/interface \
                         in the unstable side; the stable consumer depends on the abstraction.",
                        module_name
                    )),
            )
        }
        "zone_of_pain" => {
            let module_name = module.unwrap_or("?");
            Some(
                RecommendedFix::new(FixAction::ExtractInterface, project)
                    .with_confidence(0.50)
                    .with_effort(EstimatedEffort::Medium)
                    .add_step(format!(
                        "Zone of Pain: module '{}' is concrete and stable — its consumers are \
                         coupled to specifics. Extract interfaces for the most-imported concrete types.",
                        module_name
                    )),
            )
        }
        "zone_of_uselessness" => {
            let module_name = module.unwrap_or("?");
            Some(
                RecommendedFix::new(FixAction::DeleteFile, project)
                    .with_confidence(0.45)
                    .with_effort(EstimatedEffort::Small)
                    .add_step(format!(
                        "Zone of Uselessness: module '{}' is abstract and unstable — abstractions \
                         without implementations. Verify whether the abstractions are used; \
                         delete unused trait/interface files.",
                        module_name
                    )),
            )
        }
        _ => None,
    }
}

/// Build a `RecommendedFix` for a design-smell finding. Returns `None` for
/// smell types we don't know how to fix.
pub fn default_fix_for_smell(
    smell: &str,
    project: &str,
    path: &str,
    line_count: i32,
    metric_summary: &str,
) -> Option<RecommendedFix> {
    let location_range = PathRange {
        path: path.to_string(),
        start_line: 1,
        end_line: line_count.max(1) as u32,
    };
    match smell {
        "god_class" => Some(
            RecommendedFix::new(FixAction::SplitFile, project)
                .with_confidence(0.55)
                .with_effort(EstimatedEffort::Large)
                .add_location(location_range)
                .add_step(format!(
                    "{} is large and multi-purpose ({}). Invoke `recommend_module_split` (Phase 3) \
                     to get an explicit chunk-cluster→file mapping with line ranges.",
                    path, metric_summary
                )),
        ),
        "srp_violation" => Some(
            RecommendedFix::new(FixAction::SplitFile, project)
                .with_confidence(0.50)
                .with_effort(EstimatedEffort::Medium)
                .add_location(location_range)
                .add_step(format!(
                    "{} mixes multiple concerns ({}). Split along topic boundaries; \
                     `recommend_module_split` (Phase 3) emits suggested filenames.",
                    path, metric_summary
                )),
        ),
        "shotgun_surgery" => Some(
            RecommendedFix::new(FixAction::ConsolidateLogic, project)
                .with_confidence(0.50)
                .with_effort(EstimatedEffort::Medium)
                .add_location(location_range)
                .add_step(format!(
                    "{} is a hub with many co-change partners ({}). \
                     `shotgun_surgery_fix` (Phase 3) computes the absorbing centroid file \
                     and lists per-partner moves.",
                    path, metric_summary
                )),
        ),
        "stale_module" => Some(
            // Phase 1 default: prompt the engineer to verify and either delete or add tests;
            // `stale_zombie_detector` (Phase 3) replaces this with a delete-vs-test decision
            // based on graph + history evidence.
            RecommendedFix::new(FixAction::AddTest, project)
                .with_confidence(0.40)
                .with_effort(EstimatedEffort::Small)
                .add_location(location_range)
                .add_step(format!(
                    "{} hasn't changed in a long time ({}). \
                     Verify whether it's still imported (check `code_graph_edges`); \
                     if importers exist, add tests to capture current behavior; \
                     if zero importers, run `stale_zombie_detector` (Phase 3) for a delete recommendation.",
                    path, metric_summary
                )),
        ),
        "unstable_dependency" => Some(
            RecommendedFix::new(FixAction::ExtractInterface, project)
                .with_confidence(0.55)
                .with_effort(EstimatedEffort::Medium)
                .add_location(location_range)
                .add_step(format!(
                    "{} is high-fan-in and high-churn ({}). \
                     Stabilize via interface extraction: define a trait/interface that \
                     captures the consumers' contract, move impl behind it, churn no longer \
                     ripples through dependent files.",
                    path, metric_summary
                )),
        ),
        _ => None,
    }
}

/// Trim a Rust/Python/etc. extension from a relative path so a derived
/// `_port.rs` / `_test.py` filename suggestion uses the bare stem.
fn trim_extension(path: &str) -> String {
    let basename = path.rsplit('/').next().unwrap_or(path);
    let stem = basename
        .rsplit_once('.')
        .map(|(s, _)| s)
        .unwrap_or(basename);
    let dir = path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    if dir.is_empty() {
        stem.to_string()
    } else {
        format!("{}/{}", dir, stem)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn propose_function_name_picks_top_two_tokens() {
        let keywords = vec!["Validate".into(), "Email".into(), "regex".into()];
        assert_eq!(propose_function_name(&keywords), "validate_email");
    }

    #[test]
    fn propose_function_name_strips_punctuation() {
        let keywords = vec!["build-request!".into(), "headers".into()];
        assert_eq!(propose_function_name(&keywords), "buildrequest_headers");
    }

    #[test]
    fn propose_function_name_stable_fallback_on_empty() {
        assert_eq!(propose_function_name(&[]), "shared_helper");
        assert_eq!(
            propose_function_name(&["!@#".into(), "***".into()]),
            "shared_helper"
        );
    }

    #[test]
    fn infer_module_name_uses_kebab_case_with_utils_suffix() {
        let keywords = vec!["validate".into(), "schema".into()];
        assert_eq!(
            infer_module_name_from_topics(&keywords),
            "validate-schema-utils"
        );
    }

    #[test]
    fn infer_module_name_fallback_is_shared_utils() {
        assert_eq!(infer_module_name_from_topics(&[]), "shared-utils");
    }

    #[test]
    fn scan_callsites_regex_finds_function_calls() {
        let content =
            "fn foo() {\n    bar();\n    let x = bar (1);\n    baz();\n    bar(); // again\n}\n";
        let hits = scan_callsites_regex(content, "bar");
        // Lines 2, 3, 5 — `bar (1)` matches because of `\s*\(`.
        assert_eq!(hits, vec![2, 3, 5]);
    }

    #[test]
    fn scan_callsites_regex_respects_word_boundaries() {
        let content = "rebar();\n  bar();\n  bar_x();\n";
        let hits = scan_callsites_regex(content, "bar");
        // `rebar` doesn't match (no boundary), `bar()` does, `bar_x()` doesn't (boundary fails).
        assert_eq!(hits, vec![2]);
    }

    #[test]
    fn scan_callsites_regex_empty_name_returns_empty() {
        assert!(scan_callsites_regex("foo() bar()", "").is_empty());
    }

    #[test]
    fn callsites_to_file_lines_pairs_path_with_line() {
        let f = callsites_to_file_lines("src/lib.rs", &[3, 17]);
        assert_eq!(
            f,
            vec![
                FileLine {
                    path: "src/lib.rs".into(),
                    line: 3
                },
                FileLine {
                    path: "src/lib.rs".into(),
                    line: 17
                },
            ]
        );
    }

    // ---- Phase 1 builders --------------------------------------------------

    #[test]
    fn default_fix_for_violation_dispatches_on_type() {
        let cycle = default_fix_for_violation(
            "dependency_cycle",
            "myproj",
            &["a.rs".into(), "b.rs".into(), "c.rs".into()],
            None,
        )
        .expect("cycle yields a fix");
        assert_eq!(cycle.action, FixAction::ExtractInterface);
        assert_eq!(cycle.location.project, "myproj");

        let god = default_fix_for_violation("god_module", "myproj", &[], Some("src/big"))
            .expect("god_module yields a fix");
        assert_eq!(god.action, FixAction::SplitFile);

        let bid = default_fix_for_violation(
            "bidirectional_dependency",
            "myproj",
            &["x.rs".into(), "y.rs".into()],
            None,
        )
        .expect("bidirectional yields a fix");
        assert_eq!(bid.action, FixAction::InvertDependency);

        let sdp = default_fix_for_violation("sdp_violation", "myproj", &[], Some("src/data"))
            .expect("sdp yields a fix");
        assert_eq!(sdp.action, FixAction::ExtractInterface);

        let pain = default_fix_for_violation("zone_of_pain", "myproj", &[], Some("src/util"))
            .expect("zone_of_pain yields a fix");
        assert_eq!(pain.action, FixAction::ExtractInterface);

        let useless =
            default_fix_for_violation("zone_of_uselessness", "myproj", &[], Some("src/abs"))
                .expect("zone_of_uselessness yields a fix");
        assert_eq!(useless.action, FixAction::DeleteFile);

        // Unknown type → None (defensive).
        assert!(default_fix_for_violation("unknown_xyz", "myproj", &[], None).is_none());
    }

    #[test]
    fn god_module_fix_never_claims_zero_files() {
        // Regression: the god_module fix used to interpolate `files.len()`, but
        // every caller passes an empty `files` slice (the real count lives in
        // the violation's `description`), so it always printed the contradictory
        // "has 0 files". The step must name the module and the threshold rule —
        // never a bogus count.
        let fix = default_fix_for_violation("god_module", "proj", &[], Some("src/mcp"))
            .expect("god_module yields a fix");
        let steps = fix.steps.join(" ");
        assert!(
            !steps.contains("0 files"),
            "god_module fix must not claim a bogus count; got: {steps}"
        );
        assert!(
            steps.contains("src/mcp"),
            "god_module fix should name the module; got: {steps}"
        );
        assert!(
            steps.contains("file-count threshold"),
            "god_module fix should state the rule, not a count; got: {steps}"
        );
    }

    #[test]
    fn default_fix_for_violation_bidirectional_requires_two_files() {
        // Defensive: only one file → None (caller would emit no fix).
        assert!(
            default_fix_for_violation("bidirectional_dependency", "p", &["only.rs".into()], None)
                .is_some(),
            "single-file bidirectional still emits fix with placeholder for missing peer"
        );
        // Empty file list → None.
        assert!(default_fix_for_violation("bidirectional_dependency", "p", &[], None).is_none());
    }

    #[test]
    fn default_fix_for_smell_dispatches_on_type() {
        let god = default_fix_for_smell("god_class", "p", "src/foo.rs", 720, "720 lines, 8 topics")
            .expect("god_class yields a fix");
        assert_eq!(god.action, FixAction::SplitFile);
        assert_eq!(god.location.paths[0].path, "src/foo.rs");
        assert_eq!(god.location.paths[0].end_line, 720);

        let srp = default_fix_for_smell("srp_violation", "p", "src/foo.rs", 300, "5 topics")
            .expect("srp_violation yields a fix");
        assert_eq!(srp.action, FixAction::SplitFile);

        let shotgun = default_fix_for_smell(
            "shotgun_surgery",
            "p",
            "src/auth.rs",
            420,
            "11 co-change partners",
        )
        .expect("shotgun yields a fix");
        assert_eq!(shotgun.action, FixAction::ConsolidateLogic);

        let stale = default_fix_for_smell(
            "stale_module",
            "p",
            "src/legacy/foo.rs",
            150,
            "Unchanged for 800 days (150 lines)",
        )
        .expect("stale yields a fix");
        assert_eq!(stale.action, FixAction::AddTest);

        let unstable = default_fix_for_smell(
            "unstable_dependency",
            "p",
            "src/core.rs",
            500,
            "12 dependents but churn rate 4.0/month",
        )
        .expect("unstable yields a fix");
        assert_eq!(unstable.action, FixAction::ExtractInterface);

        // Unknown smell → None.
        assert!(default_fix_for_smell("unknown_smell", "p", "x.rs", 1, "x").is_none());
    }

    #[test]
    fn default_fix_for_smell_clamps_zero_line_count() {
        let f = default_fix_for_smell("god_class", "p", "x.rs", 0, "metric").expect("fix");
        // PathRange.end_line must be >= 1 (it's u32 and we want it usable).
        assert_eq!(f.location.paths[0].end_line, 1);
    }

    #[test]
    fn trim_extension_keeps_directory() {
        assert_eq!(trim_extension("a/b/c.rs"), "a/b/c");
        assert_eq!(trim_extension("c.rs"), "c");
        assert_eq!(trim_extension("a/b/no_ext"), "a/b/no_ext");
    }
}
