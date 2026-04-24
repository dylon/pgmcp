//! MCP Server implementation using rmcp.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use arc_swap::ArcSwap;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::schemars;
use rmcp::service::{NotificationContext, RequestContext};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use rmcp::{tool, tool_handler, tool_router};
use serde::Deserialize;
use sqlx::PgPool;

use crate::config::Config;
use crate::context::SystemContext;
use crate::db::DbClient;
use crate::stats::tracker::StatsTracker;

use super::logging::LogBroadcaster;
use super::tasks::TaskStore;

/// Truncate a string to at most `max_len` bytes on a valid char boundary.
pub(crate) fn truncate(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        s
    } else {
        &s[..s.floor_char_boundary(max_len)]
    }
}

// ============================================================================
// Union-Find for duplicate clustering
// ============================================================================

pub(crate) struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<usize>,
}

impl UnionFind {
    pub(crate) fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
            rank: vec![0; n],
        }
    }

    pub(crate) fn find(&mut self, x: usize) -> usize {
        if self.parent[x] != x {
            self.parent[x] = self.find(self.parent[x]);
        }
        self.parent[x]
    }

    pub(crate) fn union(&mut self, x: usize, y: usize) {
        let rx = self.find(x);
        let ry = self.find(y);
        if rx == ry {
            return;
        }
        if self.rank[rx] < self.rank[ry] {
            self.parent[rx] = ry;
        } else if self.rank[rx] > self.rank[ry] {
            self.parent[ry] = rx;
        } else {
            self.parent[ry] = rx;
            self.rank[rx] += 1;
        }
    }
}

/// Cluster duplicate file pairs using union-find.
/// Returns clusters that span at least `min_projects` distinct projects.
pub(crate) fn cluster_file_pairs(
    pairs: &[crate::db::queries::DuplicateFilePair],
    min_projects: usize,
) -> Vec<serde_json::Value> {
    use std::collections::{HashMap, HashSet};

    if pairs.is_empty() {
        return Vec::new();
    }

    // Assign each unique file_id an index
    let mut file_ids: Vec<i64> = Vec::new();
    let mut id_to_idx: HashMap<i64, usize> = HashMap::new();

    for pair in pairs {
        if let std::collections::hash_map::Entry::Vacant(e) = id_to_idx.entry(pair.file_id_a) {
            e.insert(file_ids.len());
            file_ids.push(pair.file_id_a);
        }
        if let std::collections::hash_map::Entry::Vacant(e) = id_to_idx.entry(pair.file_id_b) {
            e.insert(file_ids.len());
            file_ids.push(pair.file_id_b);
        }
    }

    // Build file metadata map
    struct FileMeta {
        path: String,
        project_name: String,
        project_id: i32,
        language: String,
        line_count: Option<i64>,
    }

    let mut meta: HashMap<i64, FileMeta> = HashMap::new();
    for pair in pairs {
        meta.entry(pair.file_id_a).or_insert_with(|| FileMeta {
            path: pair.path_a.clone(),
            project_name: pair.project_name_a.clone(),
            project_id: pair.project_id_a,
            language: pair.language.clone(),
            line_count: None,
        });
        meta.entry(pair.file_id_b).or_insert_with(|| FileMeta {
            path: pair.path_b.clone(),
            project_name: pair.project_name_b.clone(),
            project_id: pair.project_id_b,
            language: pair.language.clone(),
            line_count: None,
        });
    }

    // Union-find clustering
    let mut uf = UnionFind::new(file_ids.len());
    let mut pair_sims: HashMap<(usize, usize), f64> = HashMap::new();
    for pair in pairs {
        let ia = id_to_idx[&pair.file_id_a];
        let ib = id_to_idx[&pair.file_id_b];
        uf.union(ia, ib);
        pair_sims.insert((ia.min(ib), ia.max(ib)), pair.avg_similarity);
    }

    // Collect clusters
    let mut clusters: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..file_ids.len() {
        let root = uf.find(i);
        clusters.entry(root).or_default().push(i);
    }

    // Filter to clusters spanning min_projects and format output
    let mut result: Vec<serde_json::Value> = Vec::new();
    for members in clusters.values() {
        let mut projects: HashSet<i32> = HashSet::new();
        let mut project_names: HashSet<String> = HashSet::new();
        let mut files = Vec::new();
        let mut language = String::new();
        let mut sim_sum = 0.0f64;
        let mut sim_count = 0u64;

        for &idx in members {
            let fid = file_ids[idx];
            if let Some(m) = meta.get(&fid) {
                projects.insert(m.project_id);
                project_names.insert(m.project_name.clone());
                language = m.language.clone();

                // Extract relative_path from absolute path (last path components after project root)
                let rel_path = m
                    .path
                    .rsplit('/')
                    .take(4)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("/");
                files.push(serde_json::json!({
                    "file_id": fid,
                    "path": m.path,
                    "relative_path": rel_path,
                    "project": m.project_name,
                    "line_count": m.line_count,
                }));
            }
        }

        // Calculate average similarity across all pairs in this cluster
        for i in 0..members.len() {
            for j in (i + 1)..members.len() {
                let key = (members[i].min(members[j]), members[i].max(members[j]));
                if let Some(&sim) = pair_sims.get(&key) {
                    sim_sum += sim;
                    sim_count += 1;
                }
            }
        }

        if projects.len() < min_projects {
            continue;
        }

        let avg_sim = if sim_count > 0 {
            sim_sum / sim_count as f64
        } else {
            0.0
        };

        result.push(serde_json::json!({
            "cluster_size": members.len(),
            "projects": project_names.into_iter().collect::<Vec<_>>(),
            "project_count": projects.len(),
            "language": language,
            "avg_similarity": format!("{:.4}", avg_sim),
            "files": files,
            "representative_file": files.first(),
        }));
    }

    // Sort by project_count * avg_similarity descending
    result.sort_by(|a, b| {
        let score_a = a["project_count"].as_u64().unwrap_or(0) as f64
            * a["avg_similarity"]
                .as_str()
                .unwrap_or("0")
                .parse::<f64>()
                .unwrap_or(0.0);
        let score_b = b["project_count"].as_u64().unwrap_or(0) as f64
            * b["avg_similarity"]
                .as_str()
                .unwrap_or("0")
                .parse::<f64>()
                .unwrap_or(0.0);
        score_b
            .partial_cmp(&score_a)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    result
}

/// Infer a suggested crate name from common path segments across files.
pub(crate) fn infer_crate_name(paths: &[&str]) -> String {
    if paths.is_empty() {
        return "shared-lib".to_string();
    }

    // Find common path segments (ignoring project root differences)
    // Take the last meaningful segment that appears in most paths
    let mut segment_counts: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::new();
    for path in paths {
        let segments: std::collections::HashSet<&str> = path
            .split('/')
            .filter(|s| !s.is_empty() && *s != "src" && *s != "mod.rs" && !s.contains('.'))
            .collect();
        for seg in segments {
            *segment_counts.entry(seg).or_insert(0) += 1;
        }
    }

    // Find the segment that appears in the most paths (excluding very generic ones)
    let generic = ["lib", "main", "index", "utils", "helpers", "common"];
    segment_counts
        .into_iter()
        .filter(|(seg, count)| *count > 1 && !generic.contains(seg))
        .max_by_key(|(_, count)| *count)
        .map(|(seg, _)| seg.replace('_', "-"))
        .unwrap_or_else(|| "shared-lib".to_string())
}

/// MCP Server state.
#[derive(Clone)]
pub struct McpServer {
    /// Bundled dependencies (db, embed, stats, config, log, tasks).
    /// Tool methods access these via accessor methods (`self.db()`,
    /// `self.stats()`, etc.) which delegate to the context.
    ctx: SystemContext,
    tool_router: ToolRouter<McpServer>,
}

// Accessor methods that delegate to the SystemContext. Lets the existing
// tool method bodies keep using `self.<accessor>()` identically; the
// constructor surface and the stored state collapse to a single field.
impl McpServer {
    fn db(&self) -> &Arc<dyn DbClient> {
        self.ctx.db()
    }
    #[allow(dead_code)] // Kept for parity; tool bodies have all migrated to ctx.embed() directly.
    fn embed_source(&self) -> &crate::embed::EmbedSource {
        self.ctx.embed()
    }
    fn stats(&self) -> &Arc<StatsTracker> {
        self.ctx.stats()
    }
    #[allow(dead_code)]
    fn config(&self) -> &Arc<ArcSwap<Config>> {
        self.ctx.config()
    }
    fn log_broadcaster(&self) -> &Arc<LogBroadcaster> {
        self.ctx.log_broadcaster()
    }
    fn task_store(&self) -> &Arc<TaskStore> {
        self.ctx.task_store()
    }

    /// Expose the context for `src/mcp/tools/*.rs` free functions to
    /// receive as `&SystemContext` when migrating tool bodies out of this
    /// file.
    pub(crate) fn ctx(&self) -> &SystemContext {
        &self.ctx
    }
}

// === Tool parameter types ===

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SemanticSearchParams {
    #[schemars(description = "Search query text")]
    pub query: String,
    #[schemars(description = "Maximum number of results (default: 10)")]
    pub limit: Option<i32>,
    #[schemars(description = "Filter by programming language")]
    pub language: Option<String>,
    #[schemars(description = "Filter by project name")]
    pub project: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TextSearchParams {
    #[schemars(description = "Full-text search query")]
    pub query: String,
    #[schemars(description = "Maximum number of results (default: 10)")]
    pub limit: Option<i32>,
    #[schemars(description = "Filter by programming language")]
    pub language: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GrepParams {
    #[schemars(description = "Regex pattern to search for")]
    pub pattern: String,
    #[schemars(description = "Glob pattern to filter files (e.g. '*.rs')")]
    pub glob: Option<String>,
    #[schemars(description = "Maximum number of results (default: 10)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchCommitsParams {
    #[schemars(
        description = "Search query text (matched by semantic similarity against commit messages and diffs)"
    )]
    pub query: String,
    #[schemars(description = "Maximum number of results (default: 10)")]
    pub limit: Option<i32>,
    #[schemars(description = "Filter by project name")]
    pub project: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CompareFilesParams {
    #[schemars(description = "First file reference (project:relative_path or absolute path)")]
    pub file_a: String,
    #[schemars(description = "Second file reference (project:relative_path or absolute path)")]
    pub file_b: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindSimilarModulesParams {
    #[schemars(description = "Project name containing the module")]
    pub project: String,
    #[schemars(
        description = "Module path pattern (glob/substring match, e.g. 'work_pool' or 'src/cron')"
    )]
    pub module_path: String,
    #[schemars(description = "Minimum similarity threshold (default: 0.80)")]
    pub min_similarity: Option<f64>,
    #[schemars(description = "Maximum number of results (default: 20)")]
    pub limit: Option<i32>,
    #[schemars(description = "Filter results to a specific target project")]
    pub target_project: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindDuplicatesParams {
    #[schemars(description = "Minimum similarity threshold (default: 0.90)")]
    pub min_similarity: Option<f64>,
    #[schemars(description = "Minimum number of projects a cluster must span (default: 2)")]
    pub min_projects: Option<usize>,
    #[schemars(description = "Filter by programming language")]
    pub language: Option<String>,
    #[schemars(description = "Maximum number of clusters to return (default: 20)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RefactoringReportParams {
    #[schemars(description = "Minimum similarity threshold (default: 0.85)")]
    pub min_similarity: Option<f64>,
    #[schemars(description = "Minimum number of projects a cluster must span (default: 2)")]
    pub min_projects: Option<usize>,
    #[schemars(description = "Filter by programming language")]
    pub language: Option<String>,
    #[schemars(description = "Maximum number of candidates to return (default: 20)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TopicHierarchyFcmParams {
    /// Maximum meta-groups to return (default: 50).
    #[schemars(description = "Maximum meta-groups to return (default: 50)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DiscoverTopicsParams {
    /// Project name for intra-project analysis. Omit for inter-project (cached).
    #[schemars(
        description = "Project name for intra-project analysis. Omit for inter-project (cached global results)."
    )]
    pub project: Option<String>,
    /// Minimum chunks per topic (default: 5)
    #[schemars(description = "Minimum chunks per topic (default: 5)")]
    pub min_cluster_size: Option<i32>,
    /// Filter by programming language
    #[schemars(description = "Filter by programming language")]
    pub language: Option<String>,
    /// Maximum topics to return (default: 30)
    #[schemars(description = "Maximum topics to return (default: 30)")]
    pub limit: Option<i32>,
    /// Force recomputation even if cached results exist (default: false)
    #[schemars(description = "Force recomputation even if cached results exist (default: false)")]
    pub refresh: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindOrphansParams {
    /// Project name (optional — all projects if omitted)
    #[schemars(description = "Project name (optional — all projects if omitted)")]
    pub project: Option<String>,
    /// Filter by language
    #[schemars(description = "Filter by programming language")]
    pub language: Option<String>,
    /// Max results (default: 50)
    #[schemars(description = "Max results (default: 50)")]
    pub limit: Option<i32>,
    /// "files" for file-level summary, "chunks" for chunk-level detail (default: "files")
    #[schemars(
        description = "\"files\" for file-level summary, \"chunks\" for chunk-level detail (default: \"files\")"
    )]
    pub detail: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindMisplacedCodeParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Minimum mismatch score to report (0.0-1.0, default: 0.5)
    #[schemars(description = "Minimum mismatch score to report (0.0-1.0, default: 0.5)")]
    pub min_mismatch: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindCoupledFilesParams {
    /// Project name (required — needs git history)
    #[schemars(description = "Project name (required — needs git history)")]
    pub project: String,
    /// Minimum Jaccard coupling score (0.0-1.0, default: 0.3)
    #[schemars(description = "Minimum Jaccard coupling score (0.0-1.0, default: 0.3)")]
    pub min_coupling: Option<f64>,
    /// Minimum co-commits to consider (default: 3)
    #[schemars(description = "Minimum co-commits to consider (default: 3)")]
    pub min_commits: Option<i32>,
    /// Max results (default: 50)
    #[schemars(description = "Max results (default: 50)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TestCoverageGapsParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ComplexityHotspotsParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Max results (default: 20)
    #[schemars(description = "Max results (default: 20)")]
    pub limit: Option<i32>,
    /// Sort by: "composite", "size", "chunks", "topics", "coupling" (default: "composite")
    #[schemars(
        description = "Sort by: \"composite\", \"size\", \"chunks\", \"topics\", \"coupling\" (default: \"composite\")"
    )]
    pub sort_by: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TopicHierarchyParams {
    /// Project name (optional — global if omitted)
    #[schemars(description = "Project name (optional — global if omitted)")]
    pub project: Option<String>,
    /// Number of meta-topic groups to form (default: auto = topics/3)
    #[schemars(description = "Number of meta-topic groups to form (default: auto = topics/3)")]
    pub num_groups: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SuggestMergesParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Filter by language (default: \"markdown\", use \"*\" for all)
    #[schemars(description = "Filter by language (default: \"markdown\", use \"*\" for all)")]
    pub language: Option<String>,
    /// Minimum weighted Jaccard overlap (0.0-1.0, default: 0.4)
    #[schemars(description = "Minimum weighted Jaccard overlap (0.0-1.0, default: 0.4)")]
    pub min_overlap: Option<f64>,
    /// Maximum merge groups to return (default: 20)
    #[schemars(description = "Maximum merge groups to return (default: 20)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SuggestSplitsParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Filter by language (default: \"markdown\", use \"*\" for all)
    #[schemars(description = "Filter by language (default: \"markdown\", use \"*\" for all)")]
    pub language: Option<String>,
    /// Minimum Shannon entropy to flag as split candidate (default: 1.5)
    #[schemars(
        description = "Minimum Shannon entropy of topic distribution to flag (default: 1.5)"
    )]
    pub min_entropy: Option<f64>,
    /// Minimum distinct topics per file to flag (default: 3)
    #[schemars(description = "Minimum distinct topics to flag as split candidate (default: 3)")]
    pub min_topics: Option<i32>,
    /// Maximum results (default: 20)
    #[schemars(description = "Maximum results (default: 20)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DocCoverageGapsParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
}

// === Phase 2: Graph Analysis tool parameter types ===

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DependencyGraphParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Focus on a specific file (BFS neighborhood)
    #[schemars(
        description = "Focus on a specific file (BFS neighborhood). Relative path within the project."
    )]
    pub focus_file: Option<String>,
    /// BFS depth when focus_file is set (default: 2)
    #[schemars(description = "BFS depth when focus_file is set (default: 2)")]
    pub depth: Option<i32>,
    /// Edge types to include (default: [\"import\"])
    #[schemars(
        description = "Edge types to include: \"import\", \"co_change\", \"semantic\" (default: [\"import\"])"
    )]
    pub edge_types: Option<Vec<String>>,
    /// Output format: "summary", "edges", "dot" (default: "summary")
    #[schemars(
        description = "Output format: \"summary\" (node/edge counts), \"edges\" (edge list), \"dot\" (Graphviz DOT) (default: \"summary\")"
    )]
    pub format: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CentralityAnalysisParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Centrality metric: "pagerank", "betweenness", "degree", "all" (default: "all")
    #[schemars(
        description = "Centrality metric: \"pagerank\", \"betweenness\", \"degree\", \"all\" (default: \"all\")"
    )]
    pub metric: Option<String>,
    /// Max results (default: 20)
    #[schemars(description = "Max results (default: 20)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CommunityDetectionParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Graph type: "import", "co_change", "combined" (default: "import")
    #[schemars(
        description = "Graph type for community detection: \"import\", \"co_change\", \"combined\" (default: \"import\")"
    )]
    pub graph_type: Option<String>,
    /// Louvain resolution parameter (default: 1.0, higher = more communities)
    #[schemars(
        description = "Louvain resolution parameter (default: 1.0, higher = more communities)"
    )]
    pub resolution: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CircularDependenciesParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Maximum cycle length to report (default: 10)
    #[schemars(description = "Maximum cycle length to report (default: 10)")]
    pub max_cycle_length: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ChangeImpactAnalysisParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// File to analyze impact for (relative path)
    #[schemars(description = "File to analyze impact for (relative path within the project)")]
    pub file: String,
    /// BFS depth for transitive impact (default: 3)
    #[schemars(description = "BFS depth for transitive impact (default: 3)")]
    pub depth: Option<i32>,
    /// Include semantic similarity neighbors (default: true)
    #[schemars(description = "Include semantic similarity neighbors (default: true)")]
    pub include_semantic: Option<bool>,
}

// === Phase 3: Architecture & Design Quality tool parameter types ===

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CouplingCohesionReportParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Directory depth for module grouping (default: 2)
    #[schemars(description = "Directory depth for module grouping (default: 2)")]
    pub module_depth: Option<i32>,
    /// Sort by: "instability", "distance", "coupling", "cohesion" (default: "distance")
    #[schemars(
        description = "Sort by: \"instability\", \"distance\", \"coupling\", \"cohesion\" (default: \"distance\")"
    )]
    pub sort_by: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ArchitectureViolationsParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Optional JSON layer configuration for custom architecture rules
    #[schemars(
        description = "Optional JSON layer configuration for custom architecture rules (e.g., {\"layers\": [\"api\", \"service\", \"data\"]})"
    )]
    #[allow(dead_code)]
    pub layer_config: Option<String>,
    /// Minimum severity to report: "low", "medium", "high", "critical" (default: "medium")
    #[schemars(
        description = "Minimum severity to report: \"low\", \"medium\", \"high\", \"critical\" (default: \"medium\")"
    )]
    pub severity_threshold: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DesignSmellDetectionParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Specific smells to detect (default: all)
    #[schemars(
        description = "Specific smells to detect: \"god_class\", \"srp_violation\", \"shotgun_surgery\", \"stale_module\", \"unstable_dependency\" (default: all)"
    )]
    pub smells: Option<Vec<String>>,
    /// Max results (default: 30)
    #[schemars(description = "Max results (default: 30)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ArchitectureQualityParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Detail level: "summary", "full" (default: "summary")
    #[schemars(
        description = "Detail level: \"summary\" (scores only), \"full\" (scores + per-dimension detail) (default: \"summary\")"
    )]
    pub detail: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DesignMetricsParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Scope: "project", "module", "file" (default: "project")
    #[schemars(
        description = "Scope: \"project\" (aggregate), \"module\" (per directory), \"file\" (per file) (default: \"project\")"
    )]
    pub scope: Option<String>,
    /// Path filter for module/file scope
    #[schemars(description = "Path filter for module/file scope (directory prefix or file path)")]
    pub path: Option<String>,
    /// Max results (default: 30)
    #[schemars(description = "Max results (default: 30)")]
    pub limit: Option<i32>,
    /// Sort by: "system_complexity", "cyclomatic", "maintainability", "wmc" (default: "system_complexity")
    #[schemars(
        description = "Sort by: \"system_complexity\", \"cyclomatic\", \"maintainability\", \"wmc\" (default: \"system_complexity\")"
    )]
    pub sort_by: Option<String>,
}

// === Phase 4: ML Prediction tool parameter types (heuristic-based) ===

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BugPredictionParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Max results (default: 20)
    #[schemars(description = "Max results (default: 20)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TechnicalDebtAnalysisParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Max results (default: 30)
    #[schemars(description = "Max results (default: 30)")]
    pub limit: Option<i32>,
    /// Include TODO/FIXME/HACK scan (default: true)
    #[schemars(description = "Include TODO/FIXME/HACK scan (default: true)")]
    pub include_todos: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AnomalyDetectionParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Max anomalies to return (default: 20)
    #[schemars(description = "Max anomalies to return (default: 20)")]
    pub limit: Option<i32>,
    /// Expected contamination ratio (default: 0.05)
    #[schemars(
        description = "Expected contamination ratio, fraction of files expected to be anomalous (default: 0.05)"
    )]
    pub contamination: Option<f64>,
}

// === Phase 5: NLP & IR tool parameter types ===

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct HybridSearchParams {
    /// Search query text
    #[schemars(description = "Search query text")]
    pub query: String,
    /// Filter by project name
    #[schemars(description = "Filter by project name")]
    pub project: Option<String>,
    /// Filter by programming language
    #[schemars(description = "Filter by programming language")]
    pub language: Option<String>,
    /// Max results (default: 20)
    #[schemars(description = "Max results (default: 20)")]
    pub limit: Option<i32>,
    /// Weight for BM25/text search (default: 0.5)
    #[schemars(description = "Weight for BM25/text search results (default: 0.5)")]
    pub bm25_weight: Option<f64>,
    /// Weight for semantic search (default: 0.5)
    #[schemars(description = "Weight for semantic search results (default: 0.5)")]
    pub semantic_weight: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CodeSummarizeParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Scope: "project", "directory", "file" (default: "project")
    #[schemars(
        description = "Scope: \"project\" (whole project overview), \"directory\" (single directory), \"file\" (single file) (default: \"project\")"
    )]
    pub scope: Option<String>,
    /// Path for directory/file scope
    #[schemars(
        description = "Path for directory/file scope (directory prefix or file relative path)"
    )]
    pub path: Option<String>,
    /// Detail level: "brief", "standard", "detailed" (default: "standard")
    #[schemars(
        description = "Detail level: \"brief\", \"standard\", \"detailed\" (default: \"standard\")"
    )]
    pub detail: Option<String>,
}

// === Phase 6: Engineering Scorecard tool parameter types ===

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EngineeringScorecardParams {
    /// Project name (required)
    #[schemars(description = "Project name (required)")]
    pub project: String,
    /// Format: "full", "summary", "failures_only" (default: "full")
    #[schemars(
        description = "Format: \"full\" (all dimensions), \"summary\" (GPA only), \"failures_only\" (grade C or below) (default: \"full\")"
    )]
    pub format: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadFileParams {
    #[schemars(description = "Absolute path of the file to read")]
    pub path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProjectTreeParams {
    #[schemars(description = "Project name")]
    pub project: String,
    #[schemars(description = "Maximum directory depth (default: 5)")]
    pub depth: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FileInfoParams {
    #[schemars(description = "Absolute path of the file")]
    pub path: String,
}

#[tool_router]
impl McpServer {
    /// Create a new MCP server from a `SystemContext` bundle.
    pub fn new(ctx: SystemContext) -> Self {
        Self {
            ctx,
            tool_router: Self::tool_router(),
        }
    }

    /// Return the full tool catalog without instantiating an `McpServer`.
    /// Uses the `#[tool_router]` macro's generated `tool_router()` to list all tools.
    pub fn static_tool_catalog() -> Vec<rmcp::model::Tool> {
        Self::tool_router().list_all()
    }

    /// Escape hatch for tool methods + cron-orchestrator calls that still
    /// need a raw `&PgPool` (inline SQL or untraited `crate::db::queries`
    /// callers). Production: returns the underlying pool. With a mock
    /// backend (e.g. `MockDbClient` in tests): panics — those tools are not
    /// reachable through the trait alone and require an integration test
    /// against real Postgres.
    ///
    /// Will be removed in Phase 4 once all such call sites migrate to
    /// trait methods.
    fn pool(&self) -> &PgPool {
        self.db().pool().expect(
            "this MCP tool needs a real PgPool — wrap a sqlx::PgPool as Arc<dyn DbClient> \
             or migrate the call site to use a DbClient trait method",
        )
    }

    #[tool(
        description = "Search indexed code using semantic similarity (vector embeddings). Best for conceptual queries like 'error handling' or 'database connection setup'. Filter by project name to scope results. Use project: \"claude\" to search Claude Code session transcripts, memory files, and plans from ~/.claude/."
    )]
    async fn semantic_search(
        &self,
        Parameters(params): Parameters<SemanticSearchParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_semantic_search::tool_semantic_search(self.ctx(), params).await
    }

    #[tool(
        description = "Search indexed code using PostgreSQL full-text search. Best for exact keyword matches. Searches all indexed projects including Claude Code session transcripts (use the \"claude\" project)."
    )]
    async fn text_search(
        &self,
        Parameters(params): Parameters<TextSearchParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_text_search::tool_text_search(self.ctx(), params).await
    }

    #[tool(description = "Search indexed files using a regex pattern across file contents.")]
    async fn grep(
        &self,
        Parameters(params): Parameters<GrepParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_grep::tool_grep(self.ctx(), params).await
    }

    #[tool(description = "Read the content of an indexed file by its absolute path.")]
    async fn read_file(
        &self,
        Parameters(params): Parameters<ReadFileParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_read_file::tool_read_file(self.ctx(), params).await
    }

    #[tool(description = "List all discovered projects with file counts.")]
    async fn list_projects(&self) -> Result<CallToolResult, McpError> {
        super::tools::tool_list_projects::tool_list_projects(self.ctx()).await
    }

    #[tool(description = "Show the file tree for a project, limited by depth.")]
    async fn project_tree(
        &self,
        Parameters(params): Parameters<ProjectTreeParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_project_tree::tool_project_tree(self.ctx(), params).await
    }

    #[tool(
        description = "Get metadata about an indexed file (size, language, line count, last indexed)."
    )]
    async fn file_info(
        &self,
        Parameters(params): Parameters<FileInfoParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_file_info::tool_file_info(self.ctx(), params).await
    }

    #[tool(
        description = "Get overall indexing statistics including file counts, search counts, and pool state."
    )]
    async fn index_stats(&self) -> Result<CallToolResult, McpError> {
        super::tools::tool_index_stats::tool_index_stats(self.ctx()).await
    }

    #[tool(
        description = "Trigger a full re-index of all workspaces. Clears the existing index and restarts indexing. Can be invoked as a long-running task."
    )]
    async fn reindex(&self) -> Result<CallToolResult, McpError> {
        super::tools::tool_reindex::tool_reindex(self.ctx()).await
    }

    #[tool(
        description = "Compare two specific files by computing chunk-level vector similarity. Always real-time (no dependency on batch scan). Supports project:relative_path syntax (e.g. 'pgmcp:src/work_pool/adaptive.rs') or absolute paths. Returns overall similarity, chunk-by-chunk alignment, and a human-readable verdict."
    )]
    async fn compare_files(
        &self,
        Parameters(params): Parameters<CompareFilesParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_compare_files::tool_compare_files(self.ctx(), params).await
    }

    #[tool(
        description = "Find modules/files similar to a given one across all indexed projects. Queries the materialized similarity table (populated by periodic batch scan), falling back to listing matching files if no results found. Aggregates chunk-level similarity to file-level (avg, max, matching chunk count)."
    )]
    async fn find_similar_modules(
        &self,
        Parameters(params): Parameters<FindSimilarModulesParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_find_similar_modules::tool_find_similar_modules(self.ctx(), params).await
    }

    #[tool(
        description = "Find clusters of duplicated code across projects. Uses union-find clustering on the materialized similarity table to group highly similar files. Filters to clusters spanning min_projects+ distinct projects. Requires the similarity batch scan to have run at least once."
    )]
    async fn find_duplicates(
        &self,
        Parameters(params): Parameters<FindDuplicatesParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_find_duplicates::tool_find_duplicates(self.ctx(), params).await
    }

    #[tool(
        description = "Generate an actionable refactoring report identifying code that could be extracted into shared libraries. Builds on find_duplicates clustering with richer analysis: suggests crate names from common path segments, estimates shared lines, and ranks by project_count * avg_similarity. Requires the similarity batch scan to have run at least once."
    )]
    async fn refactoring_report(
        &self,
        Parameters(params): Parameters<RefactoringReportParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_refactoring_report::tool_refactoring_report(self.ctx(), params).await
    }

    #[tool(
        description = "Search git commit history using semantic similarity. Finds commits by meaning — query with concepts like 'fix database timeout' or 'add authentication'. Returns commit hash, author, date, subject, and matching diff/message content. Requires per-project opt-in via [git] index_history = true in .pgmcp.toml."
    )]
    async fn search_commits(
        &self,
        Parameters(params): Parameters<SearchCommitsParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_search_commits::tool_search_commits(self.ctx(), params).await
    }

    #[tool(
        description = "Discover semantic code patterns using Fuzzy C-Means clustering over chunk embeddings (Fuzzy BERTopic with c-TF-IDF labeling). With 'project' param: real-time intra-project analysis showing code patterns and DRY violation candidates. Without 'project': returns cached inter-project pattern discovery results (shared library candidates). Returns topic clusters with keyword labels, membership degrees, representative code snippets, file lists, and internal similarity scores."
    )]
    async fn discover_topics(
        &self,
        Parameters(params): Parameters<DiscoverTopicsParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_discover_topics::tool_discover_topics(self.ctx(), params).await
    }

    #[tool(
        description = "Meta-clustering hierarchy over global topic centroids (Phase 9). Returns FCM-based meta-groups where each meta-group's parent_topic_ids point to the global topics it contains. Complementary view to discover_topics — chunk-to-global-topic assignments remain authoritative for cross-document comparability."
    )]
    async fn topic_hierarchy_fcm(
        &self,
        Parameters(params): Parameters<TopicHierarchyFcmParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_topic_hierarchy_fcm::tool_topic_hierarchy_fcm(self.ctx(), params).await
    }

    #[tool(
        description = "Identify code chunks/files with low topic membership (below threshold). Orphan code may be utility functions, dead code, or candidates for refactoring. Requires discover_topics to have been run first."
    )]
    async fn find_orphans(
        &self,
        Parameters(params): Parameters<FindOrphansParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_find_orphans::tool_find_orphans(self.ctx(), params).await
    }

    #[tool(
        description = "Detect files whose semantic content doesn't match their directory context (architecture recovery). Compares each file's dominant topic against the majority topic of its directory neighbors. High membership entropy also signals misplacement. Requires discover_topics to have been run first."
    )]
    async fn find_misplaced_code(
        &self,
        Parameters(params): Parameters<FindMisplacedCodeParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_find_misplaced_code::tool_find_misplaced_code(self.ctx(), params).await
    }

    #[tool(
        description = "Find files that frequently change together in git commits (co-change coupling via Jaccard similarity). High coupling (>0.7) suggests files that should be in the same module. Requires git history indexing enabled via [git] index_history = true in .pgmcp.toml."
    )]
    async fn find_coupled_files(
        &self,
        Parameters(params): Parameters<FindCoupledFilesParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_find_coupled_files::tool_find_coupled_files(self.ctx(), params).await
    }

    #[tool(
        description = "Compare topic distribution of test files vs implementation files to find untested areas. Identifies topics with implementation code but no corresponding test coverage. Requires discover_topics to have been run first."
    )]
    async fn test_coverage_gaps(
        &self,
        Parameters(params): Parameters<TestCoverageGapsParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_test_coverage_gaps::tool_test_coverage_gaps(self.ctx(), params).await
    }

    #[tool(
        description = "Rank files by composite complexity (size, chunk count, topic diversity, coupling). Identifies refactoring candidates — files with high composite scores handle too many concerns (SRP violation)."
    )]
    async fn complexity_hotspots(
        &self,
        Parameters(params): Parameters<ComplexityHotspotsParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_complexity_hotspots::tool_complexity_hotspots(self.ctx(), params).await
    }

    #[tool(
        description = "Show how discovered topics relate hierarchically using agglomerative clustering on topic centroids. Reveals module boundaries and related topic groups. Groups with low merge distance contain highly related topics that could be combined."
    )]
    async fn topic_hierarchy(
        &self,
        Parameters(params): Parameters<TopicHierarchyParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_topic_hierarchy::tool_topic_hierarchy(self.ctx(), params).await
    }

    #[tool(
        description = "Find files within a project that cover overlapping topics and should be consolidated. Uses weighted Jaccard similarity on per-file topic distributions, clustered with union-find. Defaults to markdown files but works on any language. Requires discover_topics to have been run first."
    )]
    async fn suggest_merges(
        &self,
        Parameters(params): Parameters<SuggestMergesParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_suggest_merges::tool_suggest_merges(self.ctx(), params).await
    }

    #[tool(
        description = "Find files spanning too many distinct topics and suggest where to split them. Uses Shannon entropy of per-file topic distribution to identify candidates, then detects topic transitions aligned to heading boundaries (for markdown) or chunk boundaries. Requires discover_topics to have been run first."
    )]
    async fn suggest_splits(
        &self,
        Parameters(params): Parameters<SuggestSplitsParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_suggest_splits::tool_suggest_splits(self.ctx(), params).await
    }

    #[tool(
        description = "Identify code topics that lack corresponding documentation. Compares documentation chunks (markdown files) vs code chunks per topic. Topics with no doc coverage represent code areas with missing documentation. Requires discover_topics to have been run first."
    )]
    async fn doc_coverage_gaps(
        &self,
        Parameters(params): Parameters<DocCoverageGapsParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_doc_coverage_gaps::tool_doc_coverage_gaps(self.ctx(), params).await
    }

    // ========================================================================
    // Phase 2: Graph Analysis tools
    // ========================================================================

    #[tool(
        description = "Visualize the dependency graph for a project. Shows import relationships between files, optionally focused on a specific file's neighborhood. Supports summary (counts), edges (full edge list), and DOT (Graphviz) output formats. Requires the graph-analysis cron job to have populated code_graph_edges."
    )]
    async fn dependency_graph(
        &self,
        Parameters(params): Parameters<DependencyGraphParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_dependency_graph::tool_dependency_graph(self.ctx(), params).await
    }

    #[tool(
        description = "Rank files by centrality in the dependency graph (PageRank, betweenness, degree). High-centrality files are critical paths that affect many other files. Requires the graph-analysis cron job to have run."
    )]
    async fn centrality_analysis(
        &self,
        Parameters(params): Parameters<CentralityAnalysisParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_centrality_analysis::tool_centrality_analysis(self.ctx(), params).await
    }

    #[tool(
        description = "Detect module communities in the dependency graph using Louvain algorithm. Compares discovered communities against directory structure to reveal architectural misalignment. Requires the graph-analysis cron job to have run."
    )]
    async fn community_detection(
        &self,
        Parameters(params): Parameters<CommunityDetectionParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_community_detection::tool_community_detection(self.ctx(), params).await
    }

    #[tool(
        description = "Find circular dependency cycles in the import graph. Cycles make code harder to test, build, and understand. Uses Tarjan's SCC algorithm followed by DFS cycle extraction. Requires the graph-analysis cron job to have run."
    )]
    async fn circular_dependencies(
        &self,
        Parameters(params): Parameters<CircularDependenciesParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_circular_dependencies::tool_circular_dependencies(self.ctx(), params)
            .await
    }

    #[tool(
        description = "Analyze what files would be affected by changing a specific file. Combines import graph (reverse dependents), co-change history (files that often change together), and semantic similarity (functionally related code). Requires the graph-analysis cron job to have run."
    )]
    async fn change_impact_analysis(
        &self,
        Parameters(params): Parameters<ChangeImpactAnalysisParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_change_impact_analysis::tool_change_impact_analysis(self.ctx(), params)
            .await
    }

    // ========================================================================
    // Phase 3: Architecture & Design Quality tools
    // ========================================================================

    #[tool(
        description = "Compute Robert C. Martin's package metrics per module: Afferent Coupling (Ca), Efferent Coupling (Ce), Instability (I), Abstractness (A), Distance from Main Sequence (D*). Modules in the Zone of Pain (low A, low I) or Zone of Uselessness (high A, high I) need attention. Requires the graph-analysis cron job."
    )]
    async fn coupling_cohesion_report(
        &self,
        Parameters(params): Parameters<CouplingCohesionReportParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_coupling_cohesion_report::tool_coupling_cohesion_report(
            self.ctx(),
            params,
        )
        .await
    }

    #[tool(
        description = "Detect architecture violations: dependency cycles, god modules, bidirectional dependencies, Stable Dependencies Principle (SDP) violations, and modules in Zone of Pain/Uselessness. Returns violations grouped by severity. Requires the graph-analysis cron job."
    )]
    async fn architecture_violations(
        &self,
        Parameters(params): Parameters<ArchitectureViolationsParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_architecture_violations::tool_architecture_violations(self.ctx(), params)
            .await
    }

    #[tool(
        description = "Detect design smells: god class (high complexity + many topics), SRP violation (high topic diversity), shotgun surgery (many co-change partners), stale module (old + no changes), unstable dependency (high churn + many dependents). Uses file_metrics and topic data. Requires the graph-analysis cron job and discover_topics."
    )]
    async fn design_smell_detection(
        &self,
        Parameters(params): Parameters<DesignSmellDetectionParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_design_smell_detection::tool_design_smell_detection(self.ctx(), params)
            .await
    }

    #[tool(
        description = "Measure positive architecture quality across 10 dimensions: separation of concerns, loose coupling, SDP compliance, acyclicity, test coverage, doc coverage, code organization, module balance, API stability, and dependency health. Each scored 0-100%. Requires graph-analysis cron job and discover_topics."
    )]
    async fn architecture_quality(
        &self,
        Parameters(params): Parameters<ArchitectureQualityParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_architecture_quality::tool_architecture_quality(self.ctx(), params).await
    }

    #[tool(
        description = "Compute detailed design metrics per file: cyclomatic complexity (from branching keywords), weighted methods per class (WMC), Card & Glass system complexity, and maintainability index. Useful for identifying over-complex files that need refactoring."
    )]
    async fn design_metrics(
        &self,
        Parameters(params): Parameters<DesignMetricsParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_design_metrics::tool_design_metrics(self.ctx(), params).await
    }

    // ========================================================================
    // Phase 4: ML Prediction tools (heuristic-based, no ML dependencies)
    // ========================================================================

    #[tool(
        description = "Predict which files are most likely to contain bugs using a heuristic composite score: churn rate * cyclomatic complexity * fix commit ratio * coupling. No ML dependencies — uses precomputed file_metrics. Requires the graph-analysis cron job."
    )]
    async fn bug_prediction(
        &self,
        Parameters(params): Parameters<BugPredictionParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_bug_prediction::tool_bug_prediction(self.ctx(), params).await
    }

    #[tool(
        description = "Analyze technical debt across a project by combining TODO/FIXME/HACK density, cyclomatic complexity, test coverage gaps, module distance from Main Sequence (D*), and churn rate into a composite debt score per file. Optionally scans file content for debt markers."
    )]
    async fn technical_debt_analysis(
        &self,
        Parameters(params): Parameters<TechnicalDebtAnalysisParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_technical_debt_analysis::tool_technical_debt_analysis(self.ctx(), params)
            .await
    }

    #[tool(
        description = "Detect anomalous files using embedding distance from project centroid and metric z-scores. Outlier files may indicate abandoned experiments, copied code from other projects, or architectural inconsistencies. No ML dependencies — uses statistical distance measures."
    )]
    async fn anomaly_detection(
        &self,
        Parameters(params): Parameters<AnomalyDetectionParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_anomaly_detection::tool_anomaly_detection(self.ctx(), params).await
    }

    // ========================================================================
    // Phase 5: NLP & IR tools
    // ========================================================================

    #[tool(
        description = "Combined text + semantic search using Reciprocal Rank Fusion (RRF). Runs both BM25 full-text search and vector similarity search in parallel, then merges results with configurable weights. Best for queries that benefit from both exact keyword matching and conceptual understanding."
    )]
    async fn hybrid_search(
        &self,
        Parameters(params): Parameters<HybridSearchParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_hybrid_search::tool_hybrid_search(self.ctx(), params).await
    }

    #[tool(
        description = "Generate a structural summary of a project, directory, or file. Identifies key modules by PageRank, describes each directory's role based on topic assignments and file composition, and highlights dominant patterns. Requires the graph-analysis cron job and discover_topics."
    )]
    async fn code_summarize(
        &self,
        Parameters(params): Parameters<CodeSummarizeParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_code_summarize::tool_code_summarize(self.ctx(), params).await
    }

    // ========================================================================
    // Phase 6: Engineering Scorecard
    // ========================================================================

    #[tool(
        description = "Comprehensive engineering quality scorecard grading 10 dimensions A-F with GPA. Aggregates results from dependency analysis, architecture quality, design smells, test coverage, documentation, and code health metrics into a single actionable report with Operational Readiness Review (ORR) checklist. Requires the graph-analysis cron job and discover_topics."
    )]
    async fn engineering_scorecard(
        &self,
        Parameters(params): Parameters<EngineeringScorecardParams>,
    ) -> Result<CallToolResult, McpError> {
        super::tools::tool_engineering_scorecard::tool_engineering_scorecard(self.ctx(), params)
            .await
    }
}

// ============================================================================
// Agglomerative clustering for topic hierarchy (ndarray-accelerated)
// ============================================================================

/// Agglomerative clustering with average linkage on topic centroids.
///
/// Pairwise cosine similarities are computed as a single matrix multiplication
/// `sim = C × Cᵀ` using ndarray, which is orders of magnitude faster than
/// element-wise loops (exploits SIMD and cache-friendly memory access).
///
/// Returns (groups, dendrogram).
pub(crate) fn agglomerative_cluster(
    centroids: &[&[f32]],
    labels: &[String],
    sizes: &[i64],
    topic_ids: &[i32],
    num_groups: usize,
) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    use ndarray::Array2;

    let n = centroids.len();
    let dim = centroids[0].len();

    // Build centroid matrix (n × dim) as f64 for precision
    let mut centroid_matrix = Array2::<f64>::zeros((n, dim));
    for (i, centroid) in centroids.iter().enumerate() {
        for (j, &val) in centroid.iter().enumerate() {
            centroid_matrix[[i, j]] = val as f64;
        }
    }

    // Compute full pairwise cosine similarity matrix via matmul: sim = C × Cᵀ
    // Since centroids are L2-normalized, dot product = cosine similarity.
    let sim_matrix = centroid_matrix.dot(&centroid_matrix.t());

    // Initialize cluster-level similarity matrix from point similarity matrix.
    // UPGMA update formula maintains this incrementally: O(k) per merge instead
    // of O(|Ci|×|Cj|) member-pair recomputation.
    let mut cluster_sim: Vec<Vec<f64>> = (0..n)
        .map(|i| (0..n).map(|j| sim_matrix[[i, j]]).collect())
        .collect();
    let mut cluster_sizes: Vec<usize> = vec![1; n];

    let mut cluster_members: Vec<Vec<usize>> = (0..n).map(|i| vec![i]).collect();
    let mut dendrogram: Vec<serde_json::Value> = Vec::new();

    // Active index list: avoids scanning deactivated indices every iteration
    let mut active_indices: Vec<usize> = (0..n).collect();
    let mut step = 0;

    while active_indices.len() > num_groups {
        // Find the most similar pair among active clusters
        let mut best_sim = f64::NEG_INFINITY;
        let mut best_i = 0;
        let mut best_j = 0;

        for (ai, &i) in active_indices.iter().enumerate() {
            for &j in &active_indices[ai + 1..] {
                if cluster_sim[i][j] > best_sim {
                    best_sim = cluster_sim[i][j];
                    best_i = i;
                    best_j = j;
                }
            }
        }

        // Record dendrogram step
        step += 1;
        let all_merged: Vec<&str> = cluster_members[best_i]
            .iter()
            .chain(cluster_members[best_j].iter())
            .map(|&idx| labels[idx].as_str())
            .collect();

        dendrogram.push(serde_json::json!({
            "step": step,
            "merged": all_merged,
            "distance": format!("{:.4}", 1.0 - best_sim),
        }));

        // UPGMA update: recompute cluster_sim[best_i][k] for all active k
        let size_a = cluster_sizes[best_i];
        let size_b = cluster_sizes[best_j];
        let total = size_a + size_b;
        for &k in &active_indices {
            if k == best_i || k == best_j {
                continue;
            }
            let new_sim = (size_a as f64 * cluster_sim[best_i][k]
                + size_b as f64 * cluster_sim[best_j][k])
                / total as f64;
            cluster_sim[best_i][k] = new_sim;
            cluster_sim[k][best_i] = new_sim;
        }
        cluster_sizes[best_i] = total;

        // Merge cluster best_j into best_i
        let members_j = cluster_members[best_j].clone();
        cluster_members[best_i].extend(members_j);

        // Remove best_j from active indices
        active_indices.retain(|&x| x != best_j);
    }

    // Build output groups from remaining active clusters
    let mut groups: Vec<serde_json::Value> = Vec::new();
    for &ci in &active_indices {
        let members = &cluster_members[ci];

        let group_topics: Vec<serde_json::Value> = members
            .iter()
            .map(|&idx| {
                serde_json::json!({
                    "id": topic_ids[idx],
                    "label": labels[idx],
                    "size": sizes[idx],
                })
            })
            .collect();

        // Group label: join topic labels with " + "
        let group_label = members
            .iter()
            .map(|&idx| labels[idx].as_str())
            .collect::<Vec<_>>()
            .join(" + ");

        // Average internal distance from precomputed point-level sim_matrix
        let mut internal_sum = 0.0f64;
        let mut internal_count = 0usize;
        for i in 0..members.len() {
            for j in (i + 1)..members.len() {
                internal_sum += 1.0 - sim_matrix[[members[i], members[j]]];
                internal_count += 1;
            }
        }
        let avg_distance = if internal_count > 0 {
            internal_sum / internal_count as f64
        } else {
            0.0
        };

        groups.push(serde_json::json!({
            "group_label": group_label,
            "merge_distance": format!("{:.4}", avg_distance),
            "topic_count": members.len(),
            "topics": group_topics,
        }));
    }

    // Sort groups by size descending
    groups.sort_by(|a, b| {
        let sa = a["topic_count"].as_u64().unwrap_or(0);
        let sb = b["topic_count"].as_u64().unwrap_or(0);
        sb.cmp(&sa)
    });

    (groups, dendrogram)
}

/// Format a ClusteringSummary into the JSON response structure.
pub(crate) fn format_clustering_summary(
    summary: &crate::cron::topic_clustering::ClusteringSummary,
    limit: i32,
) -> serde_json::Value {
    let noise_pct = if summary.chunks_analyzed > 0 {
        summary.noise_chunks as f64 / summary.chunks_analyzed as f64 * 100.0
    } else {
        0.0
    };

    let topics: Vec<serde_json::Value> = summary.topics.iter().take(limit as usize).map(|t| {
        serde_json::json!({
            "id": t.cluster_index,
            "label": t.label,
            "keywords": t.keywords,
            "keyword_scores": t.keyword_scores.iter().map(|s| format!("{:.4}", s)).collect::<Vec<_>>(),
            "size": t.chunk_ids.len(),
            "files": t.file_ids.len(),
            "projects": t.project_names,
            "project_count": t.project_names.len(),
            "avg_internal_similarity": format!("{:.4}", t.avg_internal_similarity),
            "representative_files": t.top_files.iter().take(10).map(|f| serde_json::json!({
                "path": f.path,
                "project": f.project,
                "chunks": f.chunks_in_topic,
            })).collect::<Vec<_>>(),
            "representative_snippet": truncate(&t.representative_snippet, 500),
        })
    }).collect();

    serde_json::json!({
        "scope": summary.scope,
        "algorithm": "Fuzzy C-Means + c-TF-IDF",
        "params": {
            "num_clusters": summary.num_clusters,
            "fuzziness": summary.fuzziness,
            "converged": summary.converged,
            "iterations": summary.iterations,
        },
        "chunks_analyzed": summary.chunks_analyzed,
        "topics_found": summary.topics_found,
        "noise_chunks": summary.noise_chunks,
        "noise_pct": format!("{:.1}", noise_pct),
        "topics": topics,
        "guidance": "Use compare_files to examine specific file pairs within a topic. \
                     Topics with high avg_internal_similarity and multiple files indicate \
                     DRY candidates. Keywords show c-TF-IDF extracted semantic labels.",
    })
}

// ============================================================================
// CLI dispatch — call tool handlers without a running MCP session
// ============================================================================

macro_rules! dispatch_tool {
    ($self:expr, $name:expr, $args:expr, {
        $($tool_name:literal => $method:ident($params_ty:ty)),* $(,)?
    }, no_params: {
        $($np_name:literal => $np_method:ident),* $(,)?
    }) => {
        match $name {
            $(
                $tool_name => {
                    let params: $params_ty = serde_json::from_value($args)
                        .map_err(|e| McpError::invalid_params(
                            format!("Invalid parameters for '{}': {}", $tool_name, e), None
                        ))?;
                    $self.$method(Parameters(params)).await
                }
            )*
            $(
                $np_name => $self.$np_method().await,
            )*
            _ => Err(McpError::invalid_params(
                format!("Unknown tool: '{}'. Run `pgmcp tool` to list available tools.", $name), None
            ))
        }
    };
}

impl McpServer {
    /// Return the full tool catalog (name, description, input_schema) for all registered tools.
    /// Kept as an instance method for potential future use by daemon-mode code.
    #[allow(dead_code)] // Used only through MCP tool handlers and daemon code
    pub(crate) fn tool_catalog(&self) -> Vec<rmcp::model::Tool> {
        self.tool_router.list_all()
    }

    /// Dispatch a tool call by name + JSON args, bypassing the MCP transport layer.
    /// Intentionally `pub` (not `pub(crate)`) so external test crates
    /// (e.g. `pgmcp-testing/tests/`) can drive any MCP tool without
    /// depending on the rmcp transport layer.
    #[allow(dead_code)] // Used by the bin crate (src/main.rs); lib's external test consumers reach it through this.
    pub async fn call_tool_cli(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<CallToolResult, McpError> {
        dispatch_tool!(self, name, args, {
            // Search
            "semantic_search"        => semantic_search(SemanticSearchParams),
            "text_search"            => text_search(TextSearchParams),
            "grep"                   => grep(GrepParams),
            "hybrid_search"          => hybrid_search(HybridSearchParams),
            "search_commits"         => search_commits(SearchCommitsParams),
            // File info
            "read_file"              => read_file(ReadFileParams),
            "project_tree"           => project_tree(ProjectTreeParams),
            "file_info"              => file_info(FileInfoParams),
            // Similarity
            "compare_files"          => compare_files(CompareFilesParams),
            "find_similar_modules"   => find_similar_modules(FindSimilarModulesParams),
            "find_duplicates"        => find_duplicates(FindDuplicatesParams),
            "refactoring_report"     => refactoring_report(RefactoringReportParams),
            // Topics
            "discover_topics"        => discover_topics(DiscoverTopicsParams),
            "find_orphans"           => find_orphans(FindOrphansParams),
            "find_misplaced_code"    => find_misplaced_code(FindMisplacedCodeParams),
            "find_coupled_files"     => find_coupled_files(FindCoupledFilesParams),
            "test_coverage_gaps"     => test_coverage_gaps(TestCoverageGapsParams),
            "complexity_hotspots"    => complexity_hotspots(ComplexityHotspotsParams),
            "topic_hierarchy"        => topic_hierarchy(TopicHierarchyParams),
            "suggest_merges"         => suggest_merges(SuggestMergesParams),
            "suggest_splits"         => suggest_splits(SuggestSplitsParams),
            "doc_coverage_gaps"      => doc_coverage_gaps(DocCoverageGapsParams),
            // Graph
            "dependency_graph"       => dependency_graph(DependencyGraphParams),
            "centrality_analysis"    => centrality_analysis(CentralityAnalysisParams),
            "community_detection"    => community_detection(CommunityDetectionParams),
            "circular_dependencies"  => circular_dependencies(CircularDependenciesParams),
            "change_impact_analysis" => change_impact_analysis(ChangeImpactAnalysisParams),
            // Architecture
            "coupling_cohesion_report"  => coupling_cohesion_report(CouplingCohesionReportParams),
            "architecture_violations"   => architecture_violations(ArchitectureViolationsParams),
            "design_smell_detection"    => design_smell_detection(DesignSmellDetectionParams),
            "architecture_quality"      => architecture_quality(ArchitectureQualityParams),
            "design_metrics"            => design_metrics(DesignMetricsParams),
            // Prediction
            "bug_prediction"         => bug_prediction(BugPredictionParams),
            "technical_debt_analysis" => technical_debt_analysis(TechnicalDebtAnalysisParams),
            "anomaly_detection"      => anomaly_detection(AnomalyDetectionParams),
            // Advanced
            "code_summarize"         => code_summarize(CodeSummarizeParams),
            "engineering_scorecard"  => engineering_scorecard(EngineeringScorecardParams),
        }, no_params: {
            "list_projects" => list_projects,
            "index_stats"   => index_stats,
            "reindex"       => reindex,
        })
    }
}

#[tool_handler]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_completions()
                .enable_logging()
                .enable_tasks()
                .build(),
        )
        .with_server_info(
            Implementation::new("pgmcp", env!("CARGO_PKG_VERSION")),
        )
        .with_instructions(
            "pgmcp indexes source code from the user's development workspaces into PostgreSQL \
             with pgvector embeddings. It maintains a continuously-updated index of all projects.\n\n\
             WHEN TO USE pgmcp (prefer over built-in Grep/Glob/Read for these cases):\n\
             - Cross-project searches: find patterns, functions, or concepts across ALL indexed projects at once\n\
             - Semantic/conceptual queries: \"error handling patterns\", \"database connection setup\", \
               \"authentication flow\" — use semantic_search (vector similarity)\n\
             - Keyword searches across the full indexed codebase: use text_search (PostgreSQL full-text)\n\
             - Regex searches across all indexed files: use grep\n\
             - Discovering what projects exist and their structure: use list_projects, project_tree\n\
             - Reading indexed files without filesystem access: use read_file\n\
             - Checking indexing health: use index_stats\n\n\
             Built-in tools (Grep/Glob/Read) are better for single-file or single-directory operations \
             in the current working directory. pgmcp is better for broad, cross-project exploration \
             and semantic understanding of the codebase.\n\n\
             GIT HISTORY:\n\
             - search_commits: semantic search over git commit messages and diffs — find when \
               a feature was added, a bug was fixed, or how code evolved. Requires per-project \
               opt-in via [git] index_history = true in .pgmcp.toml.\n\n\
             CLAUDE SESSION HISTORY:\n\
             - The \"claude\" project indexes ~/.claude/ (session transcripts, memory files, \
               plans). Use semantic_search or text_search with project: \"claude\" to search \
               past conversations, decisions, and context from previous Claude Code sessions.\n\n\
             CROSS-PROJECT SIMILARITY ANALYSIS:\n\
             - compare_files: Compare two specific files (always real-time, no batch dependency). \
               Supports project:relative_path syntax (e.g. 'pgmcp:src/work_pool/adaptive.rs').\n\
             - find_similar_modules: Find modules similar to a given one across projects. \
               Uses the materialized similarity table (populated by periodic batch scan).\n\
             - find_duplicates: Find clusters of duplicated code across projects using \
               union-find clustering.\n\
             - refactoring_report: Actionable refactoring candidates with suggested crate names \
               and shared line estimates.\n\n\
             CODE TOPIC DISCOVERY (Fuzzy BERTopic):\n\
             - discover_topics: Discovers semantic code patterns using Fuzzy C-Means clustering \
               with c-TF-IDF keyword labeling (Fuzzy BERTopic). With project param: real-time \
               intra-project analysis (DRY violation detection). Without project: inter-project \
               pattern discovery from cached results (shared library candidates). Returns topic \
               clusters with keyword labels, membership degrees, representative code, and files.\n\n\
             CODE ANALYSIS TOOLS:\n\
             - find_orphans: Identifies code chunks/files with low topic membership. \
               Orphan code may be utility functions, dead code, or candidates for refactoring.\n\
             - find_misplaced_code: Detects files whose semantic content doesn't match their \
               directory context. Suggests architectural reorganization.\n\
             - find_coupled_files: Finds files that frequently change together in git commits \
               (co-change coupling via Jaccard similarity). Requires git history indexing.\n\
             - test_coverage_gaps: Compares topic distribution of test files vs implementation \
               files. Finds topics with no corresponding test coverage.\n\
             - complexity_hotspots: Ranks files by composite complexity (size, chunks, topic \
               diversity, coupling). Identifies refactoring candidates.\n\
             - topic_hierarchy: Shows how discovered topics relate hierarchically using \
               agglomerative clustering on topic centroids. Reveals module boundaries.\n\n\
             DOCUMENT ANALYSIS TOOLS:\n\
             - suggest_merges: Find files (default: markdown) covering overlapping topics that \
               should be consolidated. Uses weighted Jaccard on per-file topic distributions.\n\
             - suggest_splits: Find files spanning too many distinct topics and suggest split \
               points aligned to heading boundaries. Uses Shannon entropy scoring.\n\
             - doc_coverage_gaps: Identify code topics that lack corresponding markdown \
               documentation. Compares doc chunks vs code chunks per topic.\n\n\
             GRAPH ANALYSIS TOOLS:\n\
             - dependency_graph: Visualize import/co-change/semantic dependency graph with \
               focus file neighborhood, DOT output, and connected component analysis.\n\
             - centrality_analysis: Rank files by PageRank, betweenness, or degree centrality.\n\
             - community_detection: Louvain community detection on dependency graph, compared \
               against directory structure to reveal architectural misalignment.\n\
             - circular_dependencies: Find import cycles using Tarjan's SCC + DFS extraction.\n\
             - change_impact_analysis: Predict which files are affected by changing a given file \
               (import graph + co-change + semantic similarity).\n\n\
             ARCHITECTURE & DESIGN QUALITY TOOLS:\n\
             - coupling_cohesion_report: Robert C. Martin's package metrics (Ca, Ce, I, A, D*) \
               per module. Identifies Zone of Pain and Zone of Uselessness.\n\
             - architecture_violations: Checks for cycles, god modules, bidirectional deps, \
               SDP violations, and zone problems. Grouped by severity.\n\
             - design_smell_detection: Detects god class, SRP violation, shotgun surgery, \
               stale module, and unstable dependency patterns.\n\
             - architecture_quality: Scores 10 quality dimensions 0-100% with letter grades.\n\
             - design_metrics: Per-file cyclomatic complexity, WMC, Card & Glass S/D/Sy, and \
               maintainability index.\n\n\
             PREDICTIVE ANALYSIS TOOLS (heuristic-based):\n\
             - bug_prediction: Ranks files by bug-proneness score (churn * complexity * fix ratio).\n\
             - technical_debt_analysis: Composite debt score from TODO density, complexity, \
               test gaps, churn. Scans content for TODO/FIXME/HACK markers.\n\
             - anomaly_detection: Identifies outlier files using embedding distance from project \
               centroid and metric z-scores.\n\n\
             HYBRID SEARCH & SUMMARIZATION:\n\
             - hybrid_search: Combined text + semantic search using Reciprocal Rank Fusion (RRF).\n\
             - code_summarize: Structural summary of project/directory/file with key files by \
               PageRank, topic overview, and language breakdown.\n\n\
             ENGINEERING SCORECARD:\n\
             - engineering_scorecard: Comprehensive quality report grading 10 dimensions A-F \
               with GPA and Operational Readiness Review (ORR) checklist.\n\n\
             Clustering uses Fuzzy C-Means (FCM) with soft membership — chunks can belong \
             to multiple topics with different membership degrees, enabling richer analysis.",
        )
    }

    // ── Lifecycle ────────────────────────────────────────────────────────

    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        tracing::info!("Client initialized, registering peer for log broadcasting");
        self.log_broadcaster().add_peer(context.peer.clone());
    }

    // ── Completions ──────────────────────────────────────────────────────

    async fn complete(
        &self,
        request: CompleteRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CompleteResult, McpError> {
        super::completions::handle_complete(self.db().as_ref(), request).await
    }

    // ── Logging ──────────────────────────────────────────────────────────

    async fn set_level(
        &self,
        request: SetLevelRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        tracing::info!(level = ?request.level, "Client set logging level");
        self.log_broadcaster().set_level(request.level);
        Ok(())
    }

    // ── Resources ────────────────────────────────────────────────────────

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(ListResourcesResult {
            resources: vec![
                RawResource::new("pgmcp://stats", "Indexing Statistics")
                    .with_description("Current indexing statistics (JSON)")
                    .no_annotation(),
                RawResource::new("pgmcp://projects", "Indexed Projects")
                    .with_description("List of indexed projects (JSON)")
                    .no_annotation(),
            ],
            next_cursor: None,
            meta: None,
        })
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        Ok(ListResourceTemplatesResult {
            resource_templates: vec![
                RawResourceTemplate::new("pgmcp://project/{name}", "Project Info")
                    .with_description("Project details by name")
                    .no_annotation(),
                RawResourceTemplate::new("pgmcp://project/{name}/tree", "Project Tree")
                    .with_description("File tree for a project")
                    .no_annotation(),
                RawResourceTemplate::new("pgmcp://file/{path}", "File Content")
                    .with_description("Read an indexed file by relative path")
                    .no_annotation(),
            ],
            next_cursor: None,
            meta: None,
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        let uri: &str = &request.uri;

        // Static resources
        match uri {
            "pgmcp://stats" => {
                let snapshot = self.stats().snapshot();
                let json = serde_json::to_string_pretty(&snapshot)
                    .map_err(|e| McpError::internal_error(format!("{}", e), None))?;
                return Ok(ReadResourceResult::new(vec![ResourceContents::text(
                    json,
                    request.uri.clone(),
                )]));
            }
            "pgmcp://projects" => {
                let projects = self
                    .db()
                    .list_projects()
                    .await
                    .map_err(|e| McpError::internal_error(format!("{}", e), None))?;
                let json = serde_json::to_string_pretty(&projects)
                    .map_err(|e| McpError::internal_error(format!("{}", e), None))?;
                return Ok(ReadResourceResult::new(vec![ResourceContents::text(
                    json,
                    request.uri.clone(),
                )]));
            }
            _ => {}
        }

        // Templated resources
        if let Some(rest) = uri.strip_prefix("pgmcp://project/") {
            if let Some(name) = rest.strip_suffix("/tree") {
                // pgmcp://project/{name}/tree
                let paths = self
                    .db()
                    .project_tree(name, 10)
                    .await
                    .map_err(|e| McpError::internal_error(format!("{}", e), None))?;
                let tree = paths.join("\n");
                return Ok(ReadResourceResult::new(vec![ResourceContents::text(
                    tree,
                    request.uri.clone(),
                )]));
            }
            // pgmcp://project/{name}
            let name = rest;
            let projects = self
                .db()
                .list_projects()
                .await
                .map_err(|e| McpError::internal_error(format!("{}", e), None))?;
            let project = projects.into_iter().find(|p| p.name == name);
            match project {
                Some(p) => {
                    let json = serde_json::to_string_pretty(&p)
                        .map_err(|e| McpError::internal_error(format!("{}", e), None))?;
                    return Ok(ReadResourceResult::new(vec![ResourceContents::text(
                        json,
                        request.uri.clone(),
                    )]));
                }
                None => {
                    return Err(McpError::resource_not_found(
                        format!("Project not found: {}", name),
                        None,
                    ));
                }
            }
        }

        if let Some(path) = uri.strip_prefix("pgmcp://file/") {
            // pgmcp://file/{path} — search by relative_path
            let file = self
                .db()
                .read_file_by_relative_path(path)
                .await
                .map_err(|e| McpError::internal_error(format!("{}", e), None))?;
            match file {
                Some(f) => {
                    let json = serde_json::to_string_pretty(&f)
                        .map_err(|e| McpError::internal_error(format!("{}", e), None))?;
                    return Ok(ReadResourceResult::new(vec![ResourceContents::text(
                        json,
                        request.uri.clone(),
                    )]));
                }
                None => {
                    return Err(McpError::resource_not_found(
                        format!("File not found: {}", path),
                        None,
                    ));
                }
            }
        }

        Err(McpError::resource_not_found(
            format!("Unknown resource: {}", uri),
            None,
        ))
    }

    // ── Tasks ────────────────────────────────────────────────────────────

    async fn enqueue_task(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CreateTaskResult, McpError> {
        match &*request.name {
            "reindex" => {
                let (task_id, cancel_flag) = self.task_store().create_task("reindex");
                let task = self
                    .task_store()
                    .get_task(&task_id)
                    .expect("Task was just created");

                // The reindex task spawns its own future; we need an owned
                // PgPool to pass into the spawn. Until reindex itself moves
                // to a trait method, clone the pool out via the escape hatch.
                let db_pool = self.pool().clone();
                let task_store = Arc::clone(self.task_store());
                let log_broadcaster = Arc::clone(self.log_broadcaster());

                tokio::spawn(async move {
                    task_store.update_progress(&task_id, "Clearing file chunks...");
                    log_broadcaster.log(
                        LoggingLevel::Info,
                        "pgmcp::reindex",
                        serde_json::json!({"message": "Reindex task started, clearing chunks"}),
                    );

                    if cancel_flag.load(Ordering::Acquire) {
                        return;
                    }

                    if let Err(e) = sqlx::query("DELETE FROM file_chunks")
                        .execute(&db_pool)
                        .await
                    {
                        task_store.fail_task(&task_id, &format!("Failed to clear chunks: {}", e));
                        return;
                    }

                    task_store.update_progress(&task_id, "Clearing indexed files...");

                    if cancel_flag.load(Ordering::Acquire) {
                        return;
                    }

                    if let Err(e) = sqlx::query("DELETE FROM indexed_files")
                        .execute(&db_pool)
                        .await
                    {
                        task_store.fail_task(&task_id, &format!("Failed to clear files: {}", e));
                        return;
                    }

                    log_broadcaster.log(
                        LoggingLevel::Info,
                        "pgmcp::reindex",
                        serde_json::json!({"message": "Index cleared, background scanner will re-index"}),
                    );

                    task_store.complete_task(
                        &task_id,
                        serde_json::json!({
                            "message": "Index cleared. Files will be re-indexed automatically by the background scanner."
                        }),
                    );
                });

                Ok(CreateTaskResult::new(task))
            }
            other => Err(McpError::internal_error(
                format!("Task processing not supported for tool: {}", other),
                None,
            )),
        }
    }

    async fn list_tasks(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListTasksResult, McpError> {
        Ok(ListTasksResult::new(self.task_store().list_tasks()))
    }

    async fn get_task_info(
        &self,
        request: GetTaskInfoParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetTaskResult, McpError> {
        match self.task_store().get_task(&request.task_id) {
            Some(task) => Ok(GetTaskResult { meta: None, task }),
            None => Err(McpError::internal_error(
                format!("Task not found: {}", request.task_id),
                None,
            )),
        }
    }

    async fn get_task_result(
        &self,
        request: GetTaskResultParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetTaskPayloadResult, McpError> {
        match self.task_store().get_result(&request.task_id) {
            Some(result) => Ok(GetTaskPayloadResult::new(result)),
            None => {
                // Check if task exists but has no result yet
                if self.task_store().get_task(&request.task_id).is_some() {
                    Err(McpError::internal_error("Task is still in progress", None))
                } else {
                    Err(McpError::internal_error(
                        format!("Task not found: {}", request.task_id),
                        None,
                    ))
                }
            }
        }
    }

    async fn cancel_task(
        &self,
        request: CancelTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CancelTaskResult, McpError> {
        match self.task_store().cancel_task(&request.task_id) {
            Some(task) => Ok(CancelTaskResult { meta: None, task }),
            None => Err(McpError::internal_error(
                format!("Task not found: {}", request.task_id),
                None,
            )),
        }
    }
}

// Cross-crate tool unit tests live under `pgmcp-testing/tests/` to avoid
// Cargo's cyclic-dev-dep limitation (pgmcp ↔ pgmcp-testing). See the
// note in `Cargo.toml`'s `[dev-dependencies]` block.
