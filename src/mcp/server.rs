//! MCP Server implementation using rmcp.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

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

use tracing::{debug, error, info};

use crate::config::Config;
use crate::stats::tracker::StatsTracker;

use super::logging::LogBroadcaster;
use super::tasks::TaskStore;

/// Truncate a string to at most `max_len` bytes on a valid char boundary.
fn truncate(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        s
    } else {
        &s[..s.floor_char_boundary(max_len)]
    }
}

// ============================================================================
// Union-Find for duplicate clustering
// ============================================================================

struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
            rank: vec![0; n],
        }
    }

    fn find(&mut self, x: usize) -> usize {
        if self.parent[x] != x {
            self.parent[x] = self.find(self.parent[x]);
        }
        self.parent[x]
    }

    fn union(&mut self, x: usize, y: usize) {
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
fn cluster_file_pairs(
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
fn infer_crate_name(paths: &[&str]) -> String {
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
    db_pool: PgPool,
    /// Query-time embedding source: pool (daemon) or lazy (CLI).
    embed_source: crate::embed::EmbedSource,
    stats: Arc<StatsTracker>,
    #[allow(dead_code)]
    config: Arc<ArcSwap<Config>>,
    tool_router: ToolRouter<McpServer>,
    log_broadcaster: Arc<LogBroadcaster>,
    task_store: Arc<TaskStore>,
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
    /// Create a new MCP server.
    ///
    /// - `embed_source`: `EmbedSource::Pool` for daemon mode (routes through embed pool);
    ///   `EmbedSource::lazy(config)` for CLI mode (lazy init on first embedding tool call).
    pub fn new(
        db_pool: PgPool,
        embed_source: crate::embed::EmbedSource,
        stats: Arc<StatsTracker>,
        config: Arc<ArcSwap<Config>>,
        log_broadcaster: Arc<LogBroadcaster>,
        task_store: Arc<TaskStore>,
    ) -> Self {
        Self {
            db_pool,
            embed_source,
            stats,
            config,
            tool_router: Self::tool_router(),
            log_broadcaster,
            task_store,
        }
    }

    /// Return the full tool catalog without instantiating an `McpServer`.
    /// Uses the `#[tool_router]` macro's generated `tool_router()` to list all tools.
    pub fn static_tool_catalog() -> Vec<rmcp::model::Tool> {
        Self::tool_router().list_all()
    }

    #[tool(
        description = "Search indexed code using semantic similarity (vector embeddings). Best for conceptual queries like 'error handling' or 'database connection setup'. Filter by project name to scope results. Use project: \"claude\" to search Claude Code session transcripts, memory files, and plans from ~/.claude/."
    )]
    async fn semantic_search(
        &self,
        Parameters(params): Parameters<SemanticSearchParams>,
    ) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        self.stats.semantic_searches.fetch_add(1, Ordering::Relaxed);

        let limit = params.limit.unwrap_or(10);
        info!(
            tool = "semantic_search",
            query = %truncate(&params.query, 200),
            limit,
            language = params.language.as_deref().unwrap_or("*"),
            project = params.project.as_deref().unwrap_or("*"),
            "MCP tool invoked",
        );

        // Embed the query
        let embedding = self
            .embed_source
            .embed_query(&params.query)
            .await
            .map_err(|e| {
                error!(tool = "semantic_search", error = %e, "MCP tool failed");
                McpError::internal_error(format!("Embedding failed: {}", e), None)
            })?;

        let ef_search = self.config.load().vector.ef_search;
        let results = crate::db::queries::semantic_search(
            &self.db_pool,
            &embedding,
            limit,
            params.language.as_deref(),
            params.project.as_deref(),
            ef_search,
        )
        .await
        .map_err(|e| {
            error!(tool = "semantic_search", error = %e, "MCP tool failed");
            McpError::internal_error(format!("Search failed: {}", e), None)
        })?;

        let count = results.len();
        let json = serde_json::to_string_pretty(&results)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        debug!(
            tool = "semantic_search",
            results = count,
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Search indexed code using PostgreSQL full-text search. Best for exact keyword matches. Searches all indexed projects including Claude Code session transcripts (use the \"claude\" project)."
    )]
    async fn text_search(
        &self,
        Parameters(params): Parameters<TextSearchParams>,
    ) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        self.stats.text_searches.fetch_add(1, Ordering::Relaxed);

        let limit = params.limit.unwrap_or(10);
        info!(
            tool = "text_search",
            query = %truncate(&params.query, 200),
            limit,
            language = params.language.as_deref().unwrap_or("*"),
            "MCP tool invoked",
        );

        let results = crate::db::queries::text_search(
            &self.db_pool,
            &params.query,
            limit,
            params.language.as_deref(),
        )
        .await
        .map_err(|e| {
            error!(tool = "text_search", error = %e, "MCP tool failed");
            McpError::internal_error(format!("Search failed: {}", e), None)
        })?;

        let count = results.len();
        let json = serde_json::to_string_pretty(&results)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        debug!(
            tool = "text_search",
            results = count,
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(description = "Search indexed files using a regex pattern across file contents.")]
    async fn grep(
        &self,
        Parameters(params): Parameters<GrepParams>,
    ) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        self.stats.grep_searches.fetch_add(1, Ordering::Relaxed);

        let limit = params.limit.unwrap_or(10);
        info!(
            tool = "grep",
            pattern = %truncate(&params.pattern, 200),
            glob = params.glob.as_deref().unwrap_or("*"),
            limit,
            "MCP tool invoked",
        );

        let results = crate::db::queries::grep_search(
            &self.db_pool,
            &params.pattern,
            params.glob.as_deref(),
            limit,
        )
        .await
        .map_err(|e| {
            error!(tool = "grep", error = %e, "MCP tool failed");
            McpError::internal_error(format!("Grep failed: {}", e), None)
        })?;

        let count = results.len();
        let json = serde_json::to_string_pretty(&results)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        debug!(
            tool = "grep",
            results = count,
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(description = "Read the content of an indexed file by its absolute path.")]
    async fn read_file(
        &self,
        Parameters(params): Parameters<ReadFileParams>,
    ) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        info!(tool = "read_file", path = %params.path, "MCP tool invoked");

        let result = crate::db::queries::read_file(&self.db_pool, &params.path)
            .await
            .map_err(|e| {
                error!(tool = "read_file", error = %e, "MCP tool failed");
                McpError::internal_error(format!("Read failed: {}", e), None)
            })?;

        let found = result.is_some();
        debug!(
            tool = "read_file",
            found,
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );

        match result {
            Some(file) => {
                let json = serde_json::to_string_pretty(&file).map_err(|e| {
                    McpError::internal_error(format!("Serialization failed: {}", e), None)
                })?;
                Ok(CallToolResult::success(vec![Content::text(json)]))
            }
            None => Ok(CallToolResult::success(vec![Content::text(format!(
                "File not found in index: {}",
                params.path
            ))])),
        }
    }

    #[tool(description = "List all discovered projects with file counts.")]
    async fn list_projects(&self) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        info!(tool = "list_projects", "MCP tool invoked");

        let projects = crate::db::queries::list_projects(&self.db_pool)
            .await
            .map_err(|e| {
                error!(tool = "list_projects", error = %e, "MCP tool failed");
                McpError::internal_error(format!("Query failed: {}", e), None)
            })?;

        let count = projects.len();
        let json = serde_json::to_string_pretty(&projects)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        debug!(
            tool = "list_projects",
            results = count,
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(description = "Show the file tree for a project, limited by depth.")]
    async fn project_tree(
        &self,
        Parameters(params): Parameters<ProjectTreeParams>,
    ) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);

        let depth = params.depth.unwrap_or(5);
        info!(
            tool = "project_tree",
            project = %params.project,
            depth,
            "MCP tool invoked",
        );

        let paths = crate::db::queries::project_tree(&self.db_pool, &params.project, depth)
            .await
            .map_err(|e| {
                error!(tool = "project_tree", error = %e, "MCP tool failed");
                McpError::internal_error(format!("Query failed: {}", e), None)
            })?;

        let count = paths.len();
        debug!(
            tool = "project_tree",
            results = count,
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );

        let tree = paths.join("\n");
        Ok(CallToolResult::success(vec![Content::text(tree)]))
    }

    #[tool(
        description = "Get metadata about an indexed file (size, language, line count, last indexed)."
    )]
    async fn file_info(
        &self,
        Parameters(params): Parameters<FileInfoParams>,
    ) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        info!(tool = "file_info", path = %params.path, "MCP tool invoked");

        let info = crate::db::queries::file_info(&self.db_pool, &params.path)
            .await
            .map_err(|e| {
                error!(tool = "file_info", error = %e, "MCP tool failed");
                McpError::internal_error(format!("Query failed: {}", e), None)
            })?;

        let found = info.is_some();
        debug!(
            tool = "file_info",
            found,
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );

        match info {
            Some(info) => {
                let json = serde_json::to_string_pretty(&info).map_err(|e| {
                    McpError::internal_error(format!("Serialization failed: {}", e), None)
                })?;
                Ok(CallToolResult::success(vec![Content::text(json)]))
            }
            None => Ok(CallToolResult::success(vec![Content::text(format!(
                "File not found in index: {}",
                params.path
            ))])),
        }
    }

    #[tool(
        description = "Get overall indexing statistics including file counts, search counts, and pool state."
    )]
    async fn index_stats(&self) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        info!(tool = "index_stats", "MCP tool invoked");

        let snapshot = self.stats.snapshot();
        let json = serde_json::to_string_pretty(&snapshot)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        debug!(
            tool = "index_stats",
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Trigger a full re-index of all workspaces. Clears the existing index and restarts indexing. Can be invoked as a long-running task."
    )]
    async fn reindex(&self) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        info!(tool = "reindex", "MCP tool invoked");

        // Synchronous (non-task) reindex: clear index directly
        sqlx::query("DELETE FROM file_chunks")
            .execute(&self.db_pool)
            .await
            .map_err(|e| {
                error!(tool = "reindex", error = %e, "Failed to clear chunks");
                McpError::internal_error(format!("Failed to clear chunks: {}", e), None)
            })?;

        sqlx::query("DELETE FROM indexed_files")
            .execute(&self.db_pool)
            .await
            .map_err(|e| {
                error!(tool = "reindex", error = %e, "Failed to clear files");
                McpError::internal_error(format!("Failed to clear files: {}", e), None)
            })?;

        self.log_broadcaster.log(
            LoggingLevel::Info,
            "pgmcp::reindex",
            serde_json::json!({"message": "Index cleared via reindex tool"}),
        );

        debug!(
            tool = "reindex",
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );

        Ok(CallToolResult::success(vec![Content::text(
            "Index cleared. Files will be re-indexed automatically by the background scanner.",
        )]))
    }

    #[tool(
        description = "Compare two specific files by computing chunk-level vector similarity. Always real-time (no dependency on batch scan). Supports project:relative_path syntax (e.g. 'pgmcp:src/work_pool/adaptive.rs') or absolute paths. Returns overall similarity, chunk-by-chunk alignment, and a human-readable verdict."
    )]
    async fn compare_files(
        &self,
        Parameters(params): Parameters<CompareFilesParams>,
    ) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        info!(
            tool = "compare_files",
            file_a = %truncate(&params.file_a, 200),
            file_b = %truncate(&params.file_b, 200),
            "MCP tool invoked",
        );

        let ref_a = crate::db::queries::resolve_file_reference(&self.db_pool, &params.file_a)
            .await
            .map_err(|e| McpError::internal_error(format!("Resolve file_a failed: {}", e), None))?
            .ok_or_else(|| {
                McpError::internal_error(format!("File not found: {}", params.file_a), None)
            })?;

        let ref_b = crate::db::queries::resolve_file_reference(&self.db_pool, &params.file_b)
            .await
            .map_err(|e| McpError::internal_error(format!("Resolve file_b failed: {}", e), None))?
            .ok_or_else(|| {
                McpError::internal_error(format!("File not found: {}", params.file_b), None)
            })?;

        let ef_search = self.config.load().vector.ef_search;
        let pairs = crate::db::queries::compare_two_files(
            &self.db_pool,
            ref_a.file_id,
            ref_b.file_id,
            ef_search,
        )
        .await
        .map_err(|e| McpError::internal_error(format!("Comparison failed: {}", e), None))?;

        // Greedy bipartite matching: match each chunk from A to best available chunk from B
        let mut used_b = std::collections::HashSet::new();
        let mut matched_pairs = Vec::new();
        let mut total_weighted_sim = 0.0f64;
        let mut total_weight = 0.0f64;

        for pair in &pairs {
            if used_b.contains(&pair.chunk_id_b) {
                continue;
            }
            // Check if this A chunk is already matched
            if matched_pairs
                .iter()
                .any(|p: &crate::db::queries::ChunkPairSimilarity| p.chunk_id_a == pair.chunk_id_a)
            {
                continue;
            }
            used_b.insert(pair.chunk_id_b);
            let weight_a = (pair.end_line_a - pair.start_line_a + 1) as f64;
            let weight_b = (pair.end_line_b - pair.start_line_b + 1) as f64;
            let weight = (weight_a + weight_b) / 2.0;
            total_weighted_sim += pair.similarity * weight;
            total_weight += weight;
            matched_pairs.push(pair.clone());
        }

        let overall_similarity = if total_weight > 0.0 {
            total_weighted_sim / total_weight
        } else {
            0.0
        };

        let verdict = if overall_similarity >= 0.95 {
            "near-identical"
        } else if overall_similarity >= 0.85 {
            "highly similar"
        } else if overall_similarity >= 0.70 {
            "moderately similar"
        } else {
            "different"
        };

        let result = serde_json::json!({
            "file_a": {
                "path": ref_a.path,
                "project": ref_a.project_name,
                "language": ref_a.language,
                "line_count": ref_a.line_count,
            },
            "file_b": {
                "path": ref_b.path,
                "project": ref_b.project_name,
                "language": ref_b.language,
                "line_count": ref_b.line_count,
            },
            "overall_similarity": format!("{:.4}", overall_similarity),
            "verdict": verdict,
            "matched_chunks": matched_pairs.len(),
            "chunk_alignment": matched_pairs.iter().map(|p| serde_json::json!({
                "lines_a": format!("{}-{}", p.start_line_a, p.end_line_a),
                "lines_b": format!("{}-{}", p.start_line_b, p.end_line_b),
                "similarity": format!("{:.4}", p.similarity),
                "snippet_a": truncate(&p.content_a, 200),
                "snippet_b": truncate(&p.content_b, 200),
            })).collect::<Vec<_>>(),
        });

        let json = serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        debug!(
            tool = "compare_files",
            overall_similarity = %format!("{:.4}", overall_similarity),
            verdict,
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Find modules/files similar to a given one across all indexed projects. Queries the materialized similarity table (populated by periodic batch scan), falling back to listing matching files if no results found. Aggregates chunk-level similarity to file-level (avg, max, matching chunk count)."
    )]
    async fn find_similar_modules(
        &self,
        Parameters(params): Parameters<FindSimilarModulesParams>,
    ) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        let min_sim = params.min_similarity.unwrap_or(0.80);
        let limit = params.limit.unwrap_or(20);
        info!(
            tool = "find_similar_modules",
            project = %params.project,
            module_path = %params.module_path,
            min_similarity = min_sim,
            limit,
            "MCP tool invoked",
        );

        // Find files matching the module path pattern
        let source_files = crate::db::queries::find_files_by_path_pattern(
            &self.db_pool,
            &params.project,
            &params.module_path,
        )
        .await
        .map_err(|e| McpError::internal_error(format!("File lookup failed: {}", e), None))?;

        if source_files.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "No files matching '{}' found in project '{}'",
                params.module_path, params.project
            ))]));
        }

        let mut all_results = Vec::new();
        for src_file in &source_files {
            let similar = crate::db::queries::find_similar_files(
                &self.db_pool,
                src_file.file_id,
                min_sim,
                limit,
                params.target_project.as_deref(),
            )
            .await
            .map_err(|e| {
                McpError::internal_error(format!("Similarity query failed: {}", e), None)
            })?;

            for sim in similar {
                all_results.push(serde_json::json!({
                    "source_file": src_file.relative_path,
                    "source_project": src_file.project_name,
                    "similar_file": sim.path_b,
                    "similar_project": sim.project_name_b,
                    "language": sim.language,
                    "avg_similarity": format!("{:.4}", sim.avg_similarity),
                    "max_similarity": format!("{:.4}", sim.max_similarity),
                    "matching_chunks": sim.matching_chunks,
                }));
            }
        }

        // Sort by avg_similarity descending and limit
        all_results.sort_by(|a, b| {
            let sa: f64 = a["avg_similarity"]
                .as_str()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0.0);
            let sb: f64 = b["avg_similarity"]
                .as_str()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0.0);
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });
        all_results.truncate(limit as usize);

        let result = serde_json::json!({
            "source_files": source_files.iter().map(|f| &f.relative_path).collect::<Vec<_>>(),
            "source_project": params.project,
            "similar_modules": all_results,
            "result_count": all_results.len(),
        });

        let json = serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        debug!(
            tool = "find_similar_modules",
            results = all_results.len(),
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Find clusters of duplicated code across projects. Uses union-find clustering on the materialized similarity table to group highly similar files. Filters to clusters spanning min_projects+ distinct projects. Requires the similarity batch scan to have run at least once."
    )]
    async fn find_duplicates(
        &self,
        Parameters(params): Parameters<FindDuplicatesParams>,
    ) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        let min_sim = params.min_similarity.unwrap_or(0.90);
        let min_projects = params.min_projects.unwrap_or(2);
        let limit = params.limit.unwrap_or(20);
        info!(
            tool = "find_duplicates",
            min_similarity = min_sim,
            min_projects,
            language = params.language.as_deref().unwrap_or("*"),
            limit,
            "MCP tool invoked",
        );

        let pairs = crate::db::queries::find_duplicate_file_pairs(
            &self.db_pool,
            min_sim,
            params.language.as_deref(),
            limit * 5,
        )
        .await
        .map_err(|e| McpError::internal_error(format!("Duplicate query failed: {}", e), None))?;

        let clusters = cluster_file_pairs(&pairs, min_projects);
        let limited: Vec<_> = clusters.into_iter().take(limit as usize).collect();

        let json = serde_json::to_string_pretty(&limited)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        debug!(
            tool = "find_duplicates",
            clusters = limited.len(),
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Generate an actionable refactoring report identifying code that could be extracted into shared libraries. Builds on find_duplicates clustering with richer analysis: suggests crate names from common path segments, estimates shared lines, and ranks by project_count * avg_similarity. Requires the similarity batch scan to have run at least once."
    )]
    async fn refactoring_report(
        &self,
        Parameters(params): Parameters<RefactoringReportParams>,
    ) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        let min_sim = params.min_similarity.unwrap_or(0.85);
        let min_projects = params.min_projects.unwrap_or(2);
        let limit = params.limit.unwrap_or(20);
        info!(
            tool = "refactoring_report",
            min_similarity = min_sim,
            min_projects,
            language = params.language.as_deref().unwrap_or("*"),
            limit,
            "MCP tool invoked",
        );

        let pairs = crate::db::queries::find_duplicate_file_pairs(
            &self.db_pool,
            min_sim,
            params.language.as_deref(),
            limit * 5,
        )
        .await
        .map_err(|e| McpError::internal_error(format!("Duplicate query failed: {}", e), None))?;

        let clusters = cluster_file_pairs(&pairs, min_projects);

        // Enrich clusters with refactoring metadata
        let mut candidates: Vec<serde_json::Value> = Vec::new();
        for cluster in clusters.iter().take(limit as usize) {
            let empty_arr = Vec::new();
            let files = cluster["files"].as_array().unwrap_or(&empty_arr).clone();
            let projects_arr = cluster["projects"].as_array().cloned().unwrap_or_default();
            let projects: Vec<&str> = projects_arr.iter().filter_map(|v| v.as_str()).collect();
            let avg_sim: f64 = cluster["avg_similarity"]
                .as_str()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0.0);

            // Infer crate name from common path segments
            let paths: Vec<&str> = files
                .iter()
                .filter_map(|f| f["relative_path"].as_str())
                .collect();
            let suggested_name = infer_crate_name(&paths);

            // Estimate shared lines (smallest file in cluster)
            let min_lines: i64 = files
                .iter()
                .filter_map(|f| f["line_count"].as_i64())
                .min()
                .unwrap_or(0);

            let score = projects.len() as f64 * avg_sim;

            candidates.push(serde_json::json!({
                "suggested_crate_name": suggested_name,
                "language": cluster["language"],
                "projects": projects,
                "project_count": projects.len(),
                "avg_similarity": cluster["avg_similarity"],
                "estimated_shared_lines": min_lines,
                "score": format!("{:.2}", score),
                "files": files,
            }));
        }

        // Sort by score descending
        candidates.sort_by(|a, b| {
            let sa: f64 = a["score"].as_str().unwrap_or("0").parse().unwrap_or(0.0);
            let sb: f64 = b["score"].as_str().unwrap_or("0").parse().unwrap_or(0.0);
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });

        let result = serde_json::json!({
            "candidates": candidates,
            "total_candidates": candidates.len(),
            "parameters": {
                "min_similarity": min_sim,
                "min_projects": min_projects,
                "language": params.language,
            },
        });

        let json = serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        debug!(
            tool = "refactoring_report",
            candidates = candidates.len(),
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Search git commit history using semantic similarity. Finds commits by meaning — query with concepts like 'fix database timeout' or 'add authentication'. Returns commit hash, author, date, subject, and matching diff/message content. Requires per-project opt-in via [git] index_history = true in .pgmcp.toml."
    )]
    async fn search_commits(
        &self,
        Parameters(params): Parameters<SearchCommitsParams>,
    ) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        self.stats.commit_searches.fetch_add(1, Ordering::Relaxed);

        let limit = params.limit.unwrap_or(10);
        info!(
            tool = "search_commits",
            query = %truncate(&params.query, 200),
            limit,
            project = params.project.as_deref().unwrap_or("*"),
            "MCP tool invoked",
        );

        // Embed the query
        let embedding = self
            .embed_source
            .embed_query(&params.query)
            .await
            .map_err(|e| {
                error!(tool = "search_commits", error = %e, "MCP tool failed");
                McpError::internal_error(format!("Embedding failed: {}", e), None)
            })?;

        let ef_search = self.config.load().vector.ef_search;
        let results = crate::db::queries::semantic_search_commits(
            &self.db_pool,
            &embedding,
            limit,
            params.project.as_deref(),
            ef_search,
        )
        .await
        .map_err(|e| {
            error!(tool = "search_commits", error = %e, "MCP tool failed");
            McpError::internal_error(format!("Commit search failed: {}", e), None)
        })?;

        let count = results.len();
        let json = serde_json::to_string_pretty(&results)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        debug!(
            tool = "search_commits",
            results = count,
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Discover semantic code patterns using Fuzzy C-Means clustering over chunk embeddings (Fuzzy BERTopic with c-TF-IDF labeling). With 'project' param: real-time intra-project analysis showing code patterns and DRY violation candidates. Without 'project': returns cached inter-project pattern discovery results (shared library candidates). Returns topic clusters with keyword labels, membership degrees, representative code snippets, file lists, and internal similarity scores."
    )]
    async fn discover_topics(
        &self,
        Parameters(params): Parameters<DiscoverTopicsParams>,
    ) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        self.stats.topic_scans.fetch_add(1, Ordering::Relaxed);

        let min_cluster_size = params.min_cluster_size.unwrap_or(5) as usize;
        let limit = params.limit.unwrap_or(30);
        let refresh = params.refresh.unwrap_or(false);

        info!(
            tool = "discover_topics",
            project = params.project.as_deref().unwrap_or("*"),
            min_cluster_size,
            language = params.language.as_deref().unwrap_or("*"),
            limit,
            refresh,
            "MCP tool invoked",
        );

        if let Some(ref project_name) = params.project {
            // On-demand per-project scan
            let config = self.config.load();
            let summary = crate::cron::topic_clustering::run_project_topic_scan(
                &self.db_pool,
                project_name,
                &config.cron,
                min_cluster_size,
                params.language.as_deref(),
            )
            .await
            .map_err(|e| {
                error!(tool = "discover_topics", error = %e, "MCP tool failed");
                McpError::internal_error(format!("Topic scan failed: {}", e), None)
            })?;

            let result = format_clustering_summary(&summary, limit);
            let json = serde_json::to_string_pretty(&result).map_err(|e| {
                McpError::internal_error(format!("Serialization failed: {}", e), None)
            })?;

            debug!(
                tool = "discover_topics",
                topics = summary.topics_found,
                duration_ms = start.elapsed().as_millis() as u64,
                "MCP tool completed (project scan)",
            );

            Ok(CallToolResult::success(vec![Content::text(json)]))
        } else {
            // Global: refresh or load cached
            if refresh {
                let config = self.config.load();
                let stats = Arc::clone(&self.stats);
                crate::cron::topic_clustering::run_global_topic_scan(
                    &self.db_pool,
                    &config.cron,
                    &stats,
                )
                .await;
            }

            let cached = crate::db::queries::load_cached_topics(&self.db_pool, "global", limit)
                .await
                .map_err(|e| {
                    error!(tool = "discover_topics", error = %e, "MCP tool failed");
                    McpError::internal_error(format!("Load cached topics failed: {}", e), None)
                })?;

            let result = serde_json::json!({
                "scope": "global",
                "algorithm": "Fuzzy C-Means + c-TF-IDF",
                "source": if refresh { "freshly computed" } else { "cached" },
                "topics_found": cached.len(),
                "topics": cached,
                "guidance": "Use compare_files to examine specific file pairs within a topic. \
                             Topics with high avg_internal_similarity and multiple files indicate \
                             DRY candidates. Use discover_topics(project: \"name\") for real-time \
                             intra-project analysis. Keywords show c-TF-IDF extracted topic labels.",
            });

            let json = serde_json::to_string_pretty(&result).map_err(|e| {
                McpError::internal_error(format!("Serialization failed: {}", e), None)
            })?;

            debug!(
                tool = "discover_topics",
                topics = cached.len(),
                duration_ms = start.elapsed().as_millis() as u64,
                "MCP tool completed (global cached)",
            );

            Ok(CallToolResult::success(vec![Content::text(json)]))
        }
    }

    #[tool(
        description = "Meta-clustering hierarchy over global topic centroids (Phase 9). Returns FCM-based meta-groups where each meta-group's parent_topic_ids point to the global topics it contains. Complementary view to discover_topics — chunk-to-global-topic assignments remain authoritative for cross-document comparability."
    )]
    async fn topic_hierarchy_fcm(
        &self,
        Parameters(params): Parameters<TopicHierarchyFcmParams>,
    ) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        self.stats.hierarchy_scans.fetch_add(1, Ordering::Relaxed);

        let limit = params.limit.unwrap_or(50);

        info!(tool = "topic_hierarchy_fcm", limit, "MCP tool invoked",);

        #[derive(sqlx::FromRow, serde::Serialize)]
        struct HierarchyRow {
            id: i64,
            cluster_index: i32,
            label: String,
            keywords: Option<Vec<String>>,
            parent_topic_ids: Option<Vec<i64>>,
        }

        let rows = sqlx::query_as::<_, HierarchyRow>(
            "SELECT id::bigint, cluster_index, label, keywords, parent_topic_ids
             FROM code_topics
             WHERE scope = 'hierarchy'
             ORDER BY cluster_index
             LIMIT $1",
        )
        .bind(limit as i64)
        .fetch_all(&self.db_pool)
        .await
        .map_err(|e| {
            error!(tool = "topic_hierarchy_fcm", error = %e, "MCP tool failed");
            McpError::internal_error(format!("Load hierarchy failed: {}", e), None)
        })?;

        let result = serde_json::json!({
            "scope": "hierarchy",
            "algorithm": "Fuzzy C-Means on global topic centroids",
            "meta_groups_found": rows.len(),
            "meta_groups": rows,
            "guidance": "Each meta_group.parent_topic_ids lists the global topic IDs \
                         composing that meta-group. Use discover_topics without a project \
                         param to get chunk-to-global-topic assignments, and this tool to \
                         navigate the higher-level semantic hierarchy. If no rows appear, \
                         run discover_topics with refresh=true first — hierarchy is chained \
                         after every global FCM run.",
        });

        let json = serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        debug!(
            tool = "topic_hierarchy_fcm",
            meta_groups = rows.len(),
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Identify code chunks/files with low topic membership (below threshold). Orphan code may be utility functions, dead code, or candidates for refactoring. Requires discover_topics to have been run first."
    )]
    async fn find_orphans(
        &self,
        Parameters(params): Parameters<FindOrphansParams>,
    ) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        self.stats.orphan_scans.fetch_add(1, Ordering::Relaxed);

        let limit = params.limit.unwrap_or(50);
        let detail = params.detail.as_deref().unwrap_or("files");

        info!(
            tool = "find_orphans",
            project = params.project.as_deref().unwrap_or("*"),
            language = params.language.as_deref().unwrap_or("*"),
            detail,
            limit,
            "MCP tool invoked",
        );

        // Check if topics have been computed
        let has_topics = crate::db::queries::has_topic_assignments(&self.db_pool)
            .await
            .unwrap_or(false);

        if !has_topics {
            return Ok(CallToolResult::success(vec![Content::text(
                "No topic assignments found. Run discover_topics first to compute semantic \
                 clusters, then find_orphans will identify chunks not assigned to any topic.",
            )]));
        }

        let json = if detail == "chunks" {
            let chunks = crate::db::queries::find_orphan_chunks(
                &self.db_pool,
                params.project.as_deref(),
                params.language.as_deref(),
                limit,
            )
            .await
            .map_err(|e| McpError::internal_error(format!("Orphan query failed: {}", e), None))?;

            let result = serde_json::json!({
                "detail": "chunks",
                "orphan_count": chunks.len(),
                "orphans": chunks,
                "guidance": "Orphan chunks are code not assigned to any semantic topic. \
                             They may be utility functions, one-off scripts, or code needing refactoring.",
            });
            serde_json::to_string_pretty(&result).map_err(|e| {
                McpError::internal_error(format!("Serialization failed: {}", e), None)
            })?
        } else {
            let files = crate::db::queries::find_orphan_file_summary(
                &self.db_pool,
                params.project.as_deref(),
            )
            .await
            .map_err(|e| McpError::internal_error(format!("Orphan query failed: {}", e), None))?;

            let result = serde_json::json!({
                "detail": "files",
                "file_count": files.len(),
                "files": files,
                "guidance": "Files with high orphan_pct have code that doesn't fit any discovered \
                             semantic pattern. Consider refactoring or reviewing these files.",
            });
            serde_json::to_string_pretty(&result).map_err(|e| {
                McpError::internal_error(format!("Serialization failed: {}", e), None)
            })?
        };

        debug!(
            tool = "find_orphans",
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Detect files whose semantic content doesn't match their directory context (architecture recovery). Compares each file's dominant topic against the majority topic of its directory neighbors. High membership entropy also signals misplacement. Requires discover_topics to have been run first."
    )]
    async fn find_misplaced_code(
        &self,
        Parameters(params): Parameters<FindMisplacedCodeParams>,
    ) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        self.stats.misplaced_scans.fetch_add(1, Ordering::Relaxed);

        let min_mismatch = params.min_mismatch.unwrap_or(0.5);

        info!(
            tool = "find_misplaced_code",
            project = %params.project,
            min_mismatch,
            "MCP tool invoked",
        );

        let rows = crate::db::queries::load_chunk_topic_assignments_for_files(
            &self.db_pool,
            Some(&params.project),
        )
        .await
        .map_err(|e| McpError::internal_error(format!("Query failed: {}", e), None))?;

        if rows.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No topic assignments found. Run discover_topics first.",
            )]));
        }

        // Build file → dominant topic map
        let mut file_dominant: std::collections::HashMap<String, (i32, String)> =
            std::collections::HashMap::new();
        for row in &rows {
            file_dominant
                .entry(row.path.clone())
                .or_insert((row.topic_id, row.topic_label.clone()));
        }

        // Build directory → topic distribution map
        let mut dir_topics: std::collections::HashMap<
            String,
            std::collections::HashMap<i32, usize>,
        > = std::collections::HashMap::new();
        for (path, (topic_id, _)) in &file_dominant {
            let dir = path
                .rsplit_once('/')
                .map(|(d, _)| d.to_string())
                .unwrap_or_default();
            *dir_topics
                .entry(dir)
                .or_default()
                .entry(*topic_id)
                .or_insert(0) += 1;
        }

        // Score each file
        let mut misplaced: Vec<serde_json::Value> = Vec::new();
        for (path, (file_topic_id, file_topic_label)) in &file_dominant {
            let dir = path
                .rsplit_once('/')
                .map(|(d, _)| d.to_string())
                .unwrap_or_default();
            if let Some(topic_counts) = dir_topics.get(&dir) {
                let total_files: usize = topic_counts.values().sum();
                if total_files <= 1 {
                    continue; // Can't determine mismatch with only one file
                }
                let file_topic_count = topic_counts.get(file_topic_id).copied().unwrap_or(0);
                let mismatch_score = 1.0 - (file_topic_count as f64 / total_files as f64);

                if mismatch_score >= min_mismatch {
                    // Find the directory's majority topic
                    let (majority_topic_id, _) = topic_counts
                        .iter()
                        .max_by_key(|(_, count)| *count)
                        .map(|(id, count)| (*id, *count))
                        .unwrap_or((0, 0));

                    let majority_label = rows
                        .iter()
                        .find(|r| r.topic_id == majority_topic_id)
                        .map(|r| r.topic_label.as_str())
                        .unwrap_or("unknown");

                    misplaced.push(serde_json::json!({
                        "path": path,
                        "directory": dir,
                        "file_topic": file_topic_label,
                        "directory_majority_topic": majority_label,
                        "mismatch_score": format!("{:.2}", mismatch_score),
                        "files_in_directory": total_files,
                    }));
                }
            }
        }

        // Sort by mismatch score descending
        misplaced.sort_by(|a, b| {
            let sa: f64 = a["mismatch_score"]
                .as_str()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0.0);
            let sb: f64 = b["mismatch_score"]
                .as_str()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0.0);
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });

        let result = serde_json::json!({
            "project": params.project,
            "min_mismatch": min_mismatch,
            "misplaced_count": misplaced.len(),
            "misplaced_files": misplaced,
            "guidance": "Files whose semantic content doesn't match their directory context. \
                         Consider moving misplaced files to directories matching their semantic \
                         content, or investigate if they serve a cross-cutting concern.",
        });

        let json = serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        debug!(
            tool = "find_misplaced_code",
            misplaced = misplaced.len(),
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Find files that frequently change together in git commits (co-change coupling via Jaccard similarity). High coupling (>0.7) suggests files that should be in the same module. Requires git history indexing enabled via [git] index_history = true in .pgmcp.toml."
    )]
    async fn find_coupled_files(
        &self,
        Parameters(params): Parameters<FindCoupledFilesParams>,
    ) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        self.stats.coupling_scans.fetch_add(1, Ordering::Relaxed);

        let min_coupling = params.min_coupling.unwrap_or(0.3);
        let min_commits = params.min_commits.unwrap_or(3);
        let limit = params.limit.unwrap_or(50);

        info!(
            tool = "find_coupled_files",
            project = %params.project,
            min_coupling,
            min_commits,
            limit,
            "MCP tool invoked",
        );

        // Check if git_commit_files has data
        let has_data =
            crate::db::queries::has_commit_files_for_project(&self.db_pool, &params.project)
                .await
                .unwrap_or(false);

        if !has_data {
            return Ok(CallToolResult::success(vec![Content::text(
                "No git commit file data found for this project. Enable git history indexing \
                 by adding [git] index_history = true to the project's .pgmcp.toml, then wait \
                 for the git-history-index cron job to run.",
            )]));
        }

        let mut pairs = crate::db::queries::find_coupled_files(
            &self.db_pool,
            &params.project,
            min_coupling,
            min_commits,
        )
        .await
        .map_err(|e| McpError::internal_error(format!("Coupling query failed: {}", e), None))?;

        pairs.truncate(limit as usize);

        let result = serde_json::json!({
            "project": params.project,
            "min_coupling": min_coupling,
            "min_commits": min_commits,
            "pair_count": pairs.len(),
            "coupled_pairs": pairs.iter().map(|p| serde_json::json!({
                "file_a": p.file_a,
                "file_b": p.file_b,
                "co_commits": p.co_commits,
                "commits_a": p.commits_a,
                "commits_b": p.commits_b,
                "jaccard": format!("{:.4}", p.jaccard),
            })).collect::<Vec<_>>(),
            "guidance": "High coupling (>0.7) suggests files that should be in the same module. \
                         Coupling without semantic similarity may indicate hidden dependencies.",
        });

        let json = serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        debug!(
            tool = "find_coupled_files",
            pairs = pairs.len(),
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Compare topic distribution of test files vs implementation files to find untested areas. Identifies topics with implementation code but no corresponding test coverage. Requires discover_topics to have been run first."
    )]
    async fn test_coverage_gaps(
        &self,
        Parameters(params): Parameters<TestCoverageGapsParams>,
    ) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        self.stats.coverage_scans.fetch_add(1, Ordering::Relaxed);

        info!(
            tool = "test_coverage_gaps",
            project = %params.project,
            "MCP tool invoked",
        );

        let rows = crate::db::queries::get_test_topic_coverage(&self.db_pool, &params.project)
            .await
            .map_err(|e| McpError::internal_error(format!("Coverage query failed: {}", e), None))?;

        if rows.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No topic assignments found. Run discover_topics first.",
            )]));
        }

        let mut topics: Vec<serde_json::Value> = Vec::with_capacity(rows.len());
        let mut total_test_chunks: i64 = 0;
        let mut total_impl_chunks: i64 = 0;

        for row in &rows {
            total_test_chunks += row.test_chunks;
            total_impl_chunks += row.impl_chunks;

            let total = row.test_chunks + row.impl_chunks;
            let test_ratio = if total > 0 {
                row.test_chunks as f64 / total as f64
            } else {
                0.0
            };

            let status = if test_ratio > 0.3 {
                "well-tested"
            } else if test_ratio > 0.01 {
                "under-tested"
            } else {
                "untested"
            };

            topics.push(serde_json::json!({
                "topic_id": row.topic_id,
                "label": row.label,
                "impl_chunks": row.impl_chunks,
                "test_chunks": row.test_chunks,
                "test_ratio": format!("{:.2}", test_ratio),
                "status": status,
            }));
        }

        // Sort by test ratio ascending (worst first)
        topics.sort_by(|a, b| {
            let ra: f64 = a["test_ratio"]
                .as_str()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0.0);
            let rb: f64 = b["test_ratio"]
                .as_str()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0.0);
            ra.partial_cmp(&rb).unwrap_or(std::cmp::Ordering::Equal)
        });

        let result = serde_json::json!({
            "project": params.project,
            "total_impl_chunks": total_impl_chunks,
            "total_test_chunks": total_test_chunks,
            "topic_count": topics.len(),
            "topics": topics,
            "guidance": "Topics with 0% test coverage are highest priority for test development. \
                         Focus on topics with many implementation chunks but no corresponding \
                         test chunks.",
        });

        let json = serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        debug!(
            tool = "test_coverage_gaps",
            topics = topics.len(),
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Rank files by composite complexity (size, chunk count, topic diversity, coupling). Identifies refactoring candidates — files with high composite scores handle too many concerns (SRP violation)."
    )]
    async fn complexity_hotspots(
        &self,
        Parameters(params): Parameters<ComplexityHotspotsParams>,
    ) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        self.stats.complexity_scans.fetch_add(1, Ordering::Relaxed);

        let limit = params.limit.unwrap_or(20);
        let sort_by = params.sort_by.as_deref().unwrap_or("composite");

        info!(
            tool = "complexity_hotspots",
            project = %params.project,
            limit,
            sort_by,
            "MCP tool invoked",
        );

        let file_data =
            crate::db::queries::get_file_complexity_data(&self.db_pool, &params.project)
                .await
                .map_err(|e| {
                    McpError::internal_error(format!("Complexity query failed: {}", e), None)
                })?;

        if file_data.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No indexed files found for this project.",
            )]));
        }

        // Get coupling data if git history is available
        let coupling_map: std::collections::HashMap<String, (f64, usize)> = {
            let coupling_pairs =
                crate::db::queries::find_coupled_files(&self.db_pool, &params.project, 0.3, 3)
                    .await
                    .unwrap_or_default();

            let mut map: std::collections::HashMap<String, (f64, usize)> =
                std::collections::HashMap::new();
            for pair in &coupling_pairs {
                {
                    let entry = map.entry(pair.file_a.clone()).or_insert((0.0, 0));
                    if pair.jaccard > entry.0 {
                        entry.0 = pair.jaccard;
                    }
                    entry.1 += 1;
                }
                {
                    let entry = map.entry(pair.file_b.clone()).or_insert((0.0, 0));
                    if pair.jaccard > entry.0 {
                        entry.0 = pair.jaccard;
                    }
                    entry.1 += 1;
                }
            }
            map
        };

        // Find max values for normalization
        let max_chunks = file_data.iter().map(|f| f.chunk_count).max().unwrap_or(1) as f64;
        let max_topics = file_data.iter().map(|f| f.topic_count).max().unwrap_or(1) as f64;
        let max_size = file_data.iter().map(|f| f.size_bytes).max().unwrap_or(1) as f64;
        let max_coupling = coupling_map
            .values()
            .map(|(c, _)| *c)
            .fold(0.0f64, f64::max)
            .max(0.001);

        // Score each file
        let mut scored: Vec<serde_json::Value> = file_data
            .iter()
            .map(|f| {
                let (file_max_coupling, coupled_file_count) =
                    coupling_map.get(&f.path).copied().unwrap_or((0.0, 0));

                let norm_chunks = f.chunk_count as f64 / max_chunks;
                let norm_topics = f.topic_count as f64 / max_topics;
                let norm_size = f.size_bytes as f64 / max_size;
                let norm_coupling = file_max_coupling / max_coupling;

                let composite = 0.30 * norm_chunks
                    + 0.25 * norm_topics
                    + 0.25 * norm_size
                    + 0.20 * norm_coupling;

                serde_json::json!({
                    "path": f.path,
                    "language": f.language,
                    "size_bytes": f.size_bytes,
                    "chunk_count": f.chunk_count,
                    "topic_count": f.topic_count,
                    "max_coupling": format!("{:.4}", file_max_coupling),
                    "coupled_files": coupled_file_count,
                    "composite_score": format!("{:.4}", composite),
                })
            })
            .collect();

        // Sort by the selected metric
        match sort_by {
            "size" => scored.sort_by(|a, b| {
                let sa = a["size_bytes"].as_i64().unwrap_or(0);
                let sb = b["size_bytes"].as_i64().unwrap_or(0);
                sb.cmp(&sa)
            }),
            "chunks" => scored.sort_by(|a, b| {
                let sa = a["chunk_count"].as_i64().unwrap_or(0);
                let sb = b["chunk_count"].as_i64().unwrap_or(0);
                sb.cmp(&sa)
            }),
            "topics" => scored.sort_by(|a, b| {
                let sa = a["topic_count"].as_i64().unwrap_or(0);
                let sb = b["topic_count"].as_i64().unwrap_or(0);
                sb.cmp(&sa)
            }),
            "coupling" => scored.sort_by(|a, b| {
                let sa: f64 = a["max_coupling"]
                    .as_str()
                    .unwrap_or("0")
                    .parse()
                    .unwrap_or(0.0);
                let sb: f64 = b["max_coupling"]
                    .as_str()
                    .unwrap_or("0")
                    .parse()
                    .unwrap_or(0.0);
                sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
            }),
            _ => scored.sort_by(|a, b| {
                let sa: f64 = a["composite_score"]
                    .as_str()
                    .unwrap_or("0")
                    .parse()
                    .unwrap_or(0.0);
                let sb: f64 = b["composite_score"]
                    .as_str()
                    .unwrap_or("0")
                    .parse()
                    .unwrap_or(0.0);
                sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
            }),
        }

        scored.truncate(limit as usize);

        let result = serde_json::json!({
            "project": params.project,
            "sort_by": sort_by,
            "file_count": scored.len(),
            "hotspots": scored,
            "guidance": "Files with high composite scores are prime candidates for refactoring. \
                         High topic diversity suggests the file handles too many concerns (SRP violation). \
                         High coupling with many files indicates the file is a change bottleneck.",
        });

        let json = serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        debug!(
            tool = "complexity_hotspots",
            hotspots = scored.len(),
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Show how discovered topics relate hierarchically using agglomerative clustering on topic centroids. Reveals module boundaries and related topic groups. Groups with low merge distance contain highly related topics that could be combined."
    )]
    async fn topic_hierarchy(
        &self,
        Parameters(params): Parameters<TopicHierarchyParams>,
    ) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        self.stats.hierarchy_scans.fetch_add(1, Ordering::Relaxed);

        let scope = params
            .project
            .as_deref()
            .map(|p| format!("project:{}", p))
            .unwrap_or_else(|| "global".to_string());

        info!(
            tool = "topic_hierarchy",
            scope = %scope,
            num_groups = params.num_groups,
            "MCP tool invoked",
        );

        // If project specified but no cached topics, run a scan first
        if let Some(ref project_name) = params.project {
            let cached = crate::db::queries::load_cached_topics(&self.db_pool, &scope, 1)
                .await
                .unwrap_or_default();

            if cached.is_empty() {
                let config = self.config.load();
                let min_cluster_size = config.cron.topic_min_cluster_size;
                crate::cron::topic_clustering::run_project_topic_scan(
                    &self.db_pool,
                    project_name,
                    &config.cron,
                    min_cluster_size,
                    None,
                )
                .await
                .map_err(|e| McpError::internal_error(format!("Topic scan failed: {}", e), None))?;
            }
        }

        let centroids = crate::db::queries::load_topic_centroids(&self.db_pool, &scope)
            .await
            .map_err(|e| McpError::internal_error(format!("Centroid query failed: {}", e), None))?;

        if centroids.len() < 2 {
            return Ok(CallToolResult::success(vec![Content::text(
                "Need at least 2 topics for hierarchy analysis. Run discover_topics first.",
            )]));
        }

        let num_groups = params
            .num_groups
            .map(|n| n as usize)
            .unwrap_or_else(|| (centroids.len() / 3).max(2));
        let num_groups = num_groups.min(centroids.len() - 1);

        let labels: Vec<String> = centroids.iter().map(|c| c.label.clone()).collect();
        let sizes: Vec<i64> = centroids.iter().map(|c| c.chunk_count).collect();
        let topic_ids: Vec<i32> = centroids.iter().map(|c| c.topic_id).collect();
        let vecs: Vec<&[f32]> = centroids.iter().map(|c| c.centroid.as_slice()).collect();

        let (groups, dendrogram) =
            agglomerative_cluster(&vecs, &labels, &sizes, &topic_ids, num_groups);

        let result = serde_json::json!({
            "scope": scope,
            "topics_total": centroids.len(),
            "num_groups": groups.len(),
            "groups": groups,
            "dendrogram": dendrogram,
            "guidance": "Groups with low merge distance contain highly related topics that could \
                         be combined into a single module. The dendrogram shows the full merge history.",
        });

        let json = serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        debug!(
            tool = "topic_hierarchy",
            groups = groups.len(),
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Find files within a project that cover overlapping topics and should be consolidated. Uses weighted Jaccard similarity on per-file topic distributions, clustered with union-find. Defaults to markdown files but works on any language. Requires discover_topics to have been run first."
    )]
    async fn suggest_merges(
        &self,
        Parameters(params): Parameters<SuggestMergesParams>,
    ) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        self.stats.merge_scans.fetch_add(1, Ordering::Relaxed);

        let language_param = params.language.as_deref().unwrap_or("markdown");
        let language_filter = if language_param == "*" {
            None
        } else {
            Some(language_param)
        };
        let min_overlap = params.min_overlap.unwrap_or(0.4);
        let limit = params.limit.unwrap_or(20);

        info!(
            tool = "suggest_merges",
            project = %params.project,
            language = language_param,
            min_overlap,
            limit,
            "MCP tool invoked",
        );

        let rows = crate::db::queries::get_file_topic_distributions(
            &self.db_pool,
            &params.project,
            language_filter,
        )
        .await
        .map_err(|e| McpError::internal_error(format!("Query failed: {}", e), None))?;

        if rows.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No topic assignments found for the specified project/language. \
                 Run discover_topics first.",
            )]));
        }

        // Build per-file topic distributions: file_id -> Vec<(topic_id, total_membership)>
        use std::collections::HashMap;

        struct FileMeta {
            path: String,
            relative_path: String,
            line_count: i32,
            size_bytes: i64,
            topics: HashMap<i32, (f64, String)>, // topic_id -> (total_membership, label)
        }

        let mut files: HashMap<i64, FileMeta> = HashMap::new();
        for row in &rows {
            let entry = files.entry(row.file_id).or_insert_with(|| FileMeta {
                path: row.path.clone(),
                relative_path: row.relative_path.clone(),
                line_count: row.line_count,
                size_bytes: row.size_bytes,
                topics: HashMap::new(),
            });
            entry.topics.insert(
                row.topic_id,
                (row.total_membership, row.topic_label.clone()),
            );
        }

        let file_ids: Vec<i64> = files.keys().copied().collect();
        let n = file_ids.len();

        if n < 2 {
            return Ok(CallToolResult::success(vec![Content::text(
                "Need at least 2 files with topic assignments for merge analysis.",
            )]));
        }

        // Compute pairwise weighted Jaccard and collect qualifying pairs
        struct MergePair {
            file_a: i64,
            file_b: i64,
            overlap: f64,
            shared_topics: Vec<String>,
        }

        let mut qualifying_pairs: Vec<MergePair> = Vec::new();

        for i in 0..n {
            for j in (i + 1)..n {
                let fa = &files[&file_ids[i]];
                let fb = &files[&file_ids[j]];

                // Weighted Jaccard: sum(min weights) / sum(max weights) over all topics
                let mut intersection_sum = 0.0f64;
                let mut union_sum = 0.0f64;
                let mut shared = Vec::new();

                // All topic IDs from both files
                let mut all_topic_ids: std::collections::HashSet<i32> =
                    fa.topics.keys().copied().collect();
                all_topic_ids.extend(fb.topics.keys());

                for &tid in &all_topic_ids {
                    let wa = fa.topics.get(&tid).map(|(m, _)| *m).unwrap_or(0.0);
                    let wb = fb.topics.get(&tid).map(|(m, _)| *m).unwrap_or(0.0);
                    intersection_sum += wa.min(wb);
                    union_sum += wa.max(wb);

                    if wa > 0.0 && wb > 0.0 {
                        let label = fa
                            .topics
                            .get(&tid)
                            .or_else(|| fb.topics.get(&tid))
                            .map(|(_, l)| l.clone())
                            .unwrap_or_default();
                        shared.push(label);
                    }
                }

                let overlap = if union_sum > 0.0 {
                    intersection_sum / union_sum
                } else {
                    0.0
                };

                if overlap >= min_overlap {
                    qualifying_pairs.push(MergePair {
                        file_a: file_ids[i],
                        file_b: file_ids[j],
                        overlap,
                        shared_topics: shared,
                    });
                }
            }
        }

        if qualifying_pairs.is_empty() {
            let result = serde_json::json!({
                "project": params.project,
                "language": language_param,
                "merge_groups_found": 0,
                "merge_groups": [],
                "guidance": "No file pairs found with topic overlap above the threshold. \
                             Try lowering min_overlap or broadening the language filter.",
            });
            let json = serde_json::to_string_pretty(&result).map_err(|e| {
                McpError::internal_error(format!("Serialization failed: {}", e), None)
            })?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        // Cluster with UnionFind
        let mut id_to_idx: HashMap<i64, usize> = HashMap::new();
        let mut idx_file_ids: Vec<i64> = Vec::new();
        for pair in &qualifying_pairs {
            if let std::collections::hash_map::Entry::Vacant(e) = id_to_idx.entry(pair.file_a) {
                e.insert(idx_file_ids.len());
                idx_file_ids.push(pair.file_a);
            }
            if let std::collections::hash_map::Entry::Vacant(e) = id_to_idx.entry(pair.file_b) {
                e.insert(idx_file_ids.len());
                idx_file_ids.push(pair.file_b);
            }
        }

        let mut uf = UnionFind::new(idx_file_ids.len());
        let mut pair_overlaps: HashMap<(usize, usize), (f64, Vec<String>)> = HashMap::new();

        for pair in &qualifying_pairs {
            let ia = id_to_idx[&pair.file_a];
            let ib = id_to_idx[&pair.file_b];
            uf.union(ia, ib);
            pair_overlaps.insert(
                (ia.min(ib), ia.max(ib)),
                (pair.overlap, pair.shared_topics.clone()),
            );
        }

        // Collect clusters
        let mut clusters: HashMap<usize, Vec<usize>> = HashMap::new();
        for i in 0..idx_file_ids.len() {
            let root = uf.find(i);
            clusters.entry(root).or_default().push(i);
        }

        // Format merge groups
        let mut merge_groups: Vec<serde_json::Value> = Vec::new();
        for members in clusters.values() {
            if members.len() < 2 {
                continue;
            }

            let mut group_files = Vec::new();
            let mut total_lines: i64 = 0;
            let mut all_shared_topics: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            let mut overlap_sum = 0.0f64;
            let mut overlap_count = 0usize;

            for &idx in members {
                let fid = idx_file_ids[idx];
                let fm = &files[&fid];
                total_lines += fm.line_count as i64;

                let topic_labels: Vec<&str> = fm.topics.values().map(|(_, l)| l.as_str()).collect();

                group_files.push(serde_json::json!({
                    "path": fm.path,
                    "relative_path": fm.relative_path,
                    "line_count": fm.line_count,
                    "size_bytes": fm.size_bytes,
                    "topic_count": fm.topics.len(),
                    "topics": topic_labels,
                }));
            }

            for i in 0..members.len() {
                for j in (i + 1)..members.len() {
                    let key = (members[i].min(members[j]), members[i].max(members[j]));
                    if let Some((ov, shared)) = pair_overlaps.get(&key) {
                        overlap_sum += ov;
                        overlap_count += 1;
                        all_shared_topics.extend(shared.iter().cloned());
                    }
                }
            }

            let avg_overlap = if overlap_count > 0 {
                overlap_sum / overlap_count as f64
            } else {
                0.0
            };

            let shared_vec: Vec<String> = all_shared_topics.into_iter().collect();

            merge_groups.push(serde_json::json!({
                "files": group_files,
                "shared_topics": shared_vec,
                "avg_overlap": format!("{:.4}", avg_overlap),
                "total_line_count": total_lines,
                "file_count": members.len(),
            }));
        }

        // Sort by avg_overlap descending
        merge_groups.sort_by(|a, b| {
            let sa: f64 = a["avg_overlap"]
                .as_str()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0.0);
            let sb: f64 = b["avg_overlap"]
                .as_str()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0.0);
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });
        merge_groups.truncate(limit as usize);

        let result = serde_json::json!({
            "project": params.project,
            "language": language_param,
            "min_overlap": min_overlap,
            "merge_groups_found": merge_groups.len(),
            "merge_groups": merge_groups,
            "guidance": "Files in the same merge group cover overlapping topics. \
                         Consider consolidating them to reduce documentation fragmentation. \
                         High avg_overlap indicates redundant topic coverage.",
        });

        let json = serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        debug!(
            tool = "suggest_merges",
            groups = merge_groups.len(),
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Find files spanning too many distinct topics and suggest where to split them. Uses Shannon entropy of per-file topic distribution to identify candidates, then detects topic transitions aligned to heading boundaries (for markdown) or chunk boundaries. Requires discover_topics to have been run first."
    )]
    async fn suggest_splits(
        &self,
        Parameters(params): Parameters<SuggestSplitsParams>,
    ) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        self.stats.split_scans.fetch_add(1, Ordering::Relaxed);

        let language_param = params.language.as_deref().unwrap_or("markdown");
        let language_filter = if language_param == "*" {
            None
        } else {
            Some(language_param)
        };
        let min_entropy = params.min_entropy.unwrap_or(1.5);
        let min_topics = params.min_topics.unwrap_or(3) as usize;
        let limit = params.limit.unwrap_or(20);

        info!(
            tool = "suggest_splits",
            project = %params.project,
            language = language_param,
            min_entropy,
            min_topics,
            limit,
            "MCP tool invoked",
        );

        let rows = crate::db::queries::get_chunk_topic_details(
            &self.db_pool,
            &params.project,
            language_filter,
        )
        .await
        .map_err(|e| McpError::internal_error(format!("Query failed: {}", e), None))?;

        if rows.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No topic assignments found for the specified project/language. \
                 Run discover_topics first.",
            )]));
        }

        // Group by file
        use std::collections::HashMap;

        struct FileChunkInfo {
            path: String,
            relative_path: String,
            language: String,
            line_count: i32,
            size_bytes: i64,
            chunks: Vec<ChunkEntry>,
        }

        struct ChunkEntry {
            chunk_index: i32,
            start_line: i32,
            content: String,
            // topic assignments for this chunk, sorted by membership_score descending
            topics: Vec<(i32, String, f64)>, // (topic_id, label, membership_score)
        }

        let mut file_map: HashMap<i64, FileChunkInfo> = HashMap::new();

        for row in &rows {
            let entry = file_map
                .entry(row.file_id)
                .or_insert_with(|| FileChunkInfo {
                    path: row.path.clone(),
                    relative_path: row.relative_path.clone(),
                    language: row.language.clone(),
                    line_count: row.line_count,
                    size_bytes: row.size_bytes,
                    chunks: Vec::new(),
                });

            // Find or create chunk entry
            if let Some(chunk) = entry
                .chunks
                .iter_mut()
                .find(|c| c.chunk_index == row.chunk_index)
            {
                chunk
                    .topics
                    .push((row.topic_id, row.topic_label.clone(), row.membership_score));
            } else {
                entry.chunks.push(ChunkEntry {
                    chunk_index: row.chunk_index,
                    start_line: row.start_line,
                    content: row.chunk_content.clone(),
                    topics: vec![(row.topic_id, row.topic_label.clone(), row.membership_score)],
                });
            }
        }

        // Sort chunks within each file by chunk_index
        for info in file_map.values_mut() {
            info.chunks.sort_by_key(|c| c.chunk_index);
        }

        // Compute entropy and filter candidates
        let heading_re = regex::Regex::new(r"^(#{1,6})\s+(.+)$").expect("valid heading regex");

        let mut candidates: Vec<serde_json::Value> = Vec::new();

        for info in file_map.values() {
            // Aggregate topic distribution across all chunks in this file
            let mut topic_membership: HashMap<i32, (f64, String)> = HashMap::new();
            for chunk in &info.chunks {
                for &(tid, ref label, score) in &chunk.topics {
                    let entry = topic_membership.entry(tid).or_insert((0.0, label.clone()));
                    entry.0 += score;
                }
            }

            let distinct_topics = topic_membership.len();
            if distinct_topics < min_topics {
                continue;
            }

            // Shannon entropy
            let total_membership: f64 = topic_membership.values().map(|(m, _)| m).sum();
            if total_membership <= 0.0 {
                continue;
            }

            let mut entropy = 0.0f64;
            let mut topic_dist: Vec<serde_json::Value> = Vec::new();

            for (tid, (membership, label)) in &topic_membership {
                let p = membership / total_membership;
                if p > 0.0 {
                    entropy -= p * p.log2();
                }
                topic_dist.push(serde_json::json!({
                    "topic_id": tid,
                    "topic": label,
                    "membership": format!("{:.2}", membership),
                    "proportion": format!("{:.2}", p),
                }));
            }

            if entropy < min_entropy {
                continue;
            }

            // Sort topic distribution by proportion descending
            topic_dist.sort_by(|a, b| {
                let pa: f64 = a["proportion"]
                    .as_str()
                    .unwrap_or("0")
                    .parse()
                    .unwrap_or(0.0);
                let pb: f64 = b["proportion"]
                    .as_str()
                    .unwrap_or("0")
                    .parse()
                    .unwrap_or(0.0);
                pb.partial_cmp(&pa).unwrap_or(std::cmp::Ordering::Equal)
            });

            // Detect topic transitions (dominant topic changes between consecutive chunks)
            let mut suggested_splits: Vec<serde_json::Value> = Vec::new();

            let mut prev_dominant: Option<(i32, String)> = None;
            for chunk in &info.chunks {
                if let Some(&(tid, ref label, _)) = chunk.topics.first() {
                    if let Some((prev_tid, ref prev_label)) = prev_dominant
                        && tid != prev_tid
                    {
                        // Topic transition — look for nearest heading
                        let transition_line = chunk.start_line;

                        // Search backward through this chunk and the previous for headings
                        let mut nearest_heading: Option<(i32, String)> = None;
                        for line in chunk.content.lines() {
                            if let Some(caps) = heading_re.captures(line) {
                                let heading_text = caps
                                    .get(2)
                                    .map(|m| m.as_str().to_string())
                                    .unwrap_or_default();
                                nearest_heading = Some((chunk.start_line, heading_text));
                                break;
                            }
                        }

                        // Generate suggested filename from heading
                        let suggested_filename = nearest_heading.as_ref().map(|(_, text)| {
                            let slug: String = text
                                .to_lowercase()
                                .chars()
                                .map(|c| if c.is_alphanumeric() { c } else { '-' })
                                .collect();
                            let slug = slug.trim_matches('-').to_string();
                            // Collapse consecutive dashes
                            let mut result = String::with_capacity(slug.len());
                            let mut prev_dash = false;
                            for c in slug.chars() {
                                if c == '-' {
                                    if !prev_dash {
                                        result.push(c);
                                    }
                                    prev_dash = true;
                                } else {
                                    result.push(c);
                                    prev_dash = false;
                                }
                            }
                            format!("{}.md", result)
                        });

                        suggested_splits.push(serde_json::json!({
                            "transition_line": transition_line,
                            "topic_before": prev_label,
                            "topic_after": label,
                            "nearest_heading": nearest_heading.as_ref().map(|(_, h)| h.as_str()),
                            "heading_line": nearest_heading.as_ref().map(|(l, _)| l),
                            "suggested_filename": suggested_filename,
                        }));
                    }
                    prev_dominant = Some((tid, label.clone()));
                }
            }

            candidates.push(serde_json::json!({
                "path": info.path,
                "relative_path": info.relative_path,
                "language": info.language,
                "line_count": info.line_count,
                "size_bytes": info.size_bytes,
                "topic_count": distinct_topics,
                "entropy": format!("{:.2}", entropy),
                "topic_distribution": topic_dist,
                "topic_transitions": suggested_splits.len(),
                "suggested_splits": suggested_splits,
            }));
        }

        // Sort by entropy descending
        candidates.sort_by(|a, b| {
            let ea: f64 = a["entropy"].as_str().unwrap_or("0").parse().unwrap_or(0.0);
            let eb: f64 = b["entropy"].as_str().unwrap_or("0").parse().unwrap_or(0.0);
            eb.partial_cmp(&ea).unwrap_or(std::cmp::Ordering::Equal)
        });
        candidates.truncate(limit as usize);

        let result = serde_json::json!({
            "project": params.project,
            "language": language_param,
            "min_entropy": min_entropy,
            "min_topics": min_topics,
            "split_candidates_found": candidates.len(),
            "candidates": candidates,
            "guidance": "Files with high entropy span many distinct topics. Split at heading \
                         boundaries that align with topic transitions for clean decomposition. \
                         Files with entropy > 2.0 are strong split candidates.",
        });

        let json = serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        debug!(
            tool = "suggest_splits",
            candidates = candidates.len(),
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Identify code topics that lack corresponding documentation. Compares documentation chunks (markdown files) vs code chunks per topic. Topics with no doc coverage represent code areas with missing documentation. Requires discover_topics to have been run first."
    )]
    async fn doc_coverage_gaps(
        &self,
        Parameters(params): Parameters<DocCoverageGapsParams>,
    ) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        self.stats
            .doc_coverage_scans
            .fetch_add(1, Ordering::Relaxed);

        info!(
            tool = "doc_coverage_gaps",
            project = %params.project,
            "MCP tool invoked",
        );

        let rows = crate::db::queries::get_doc_topic_coverage(&self.db_pool, &params.project)
            .await
            .map_err(|e| McpError::internal_error(format!("Coverage query failed: {}", e), None))?;

        if rows.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No topic assignments found. Run discover_topics first.",
            )]));
        }

        let mut topics: Vec<serde_json::Value> = Vec::with_capacity(rows.len());
        let mut total_doc_chunks: i64 = 0;
        let mut total_code_chunks: i64 = 0;

        for row in &rows {
            total_doc_chunks += row.doc_chunks;
            total_code_chunks += row.code_chunks;

            let total = row.doc_chunks + row.code_chunks;
            let doc_ratio = if total > 0 {
                row.doc_chunks as f64 / total as f64
            } else {
                0.0
            };

            let status = if doc_ratio > 0.30 {
                "well-documented"
            } else if doc_ratio > 0.05 {
                "under-documented"
            } else {
                "undocumented"
            };

            topics.push(serde_json::json!({
                "topic_id": row.topic_id,
                "label": row.label,
                "keywords": row.keywords,
                "doc_chunks": row.doc_chunks,
                "code_chunks": row.code_chunks,
                "doc_ratio": format!("{:.2}", doc_ratio),
                "status": status,
            }));
        }

        // Sort by doc_ratio ascending (worst first)
        topics.sort_by(|a, b| {
            let ra: f64 = a["doc_ratio"]
                .as_str()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0.0);
            let rb: f64 = b["doc_ratio"]
                .as_str()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0.0);
            ra.partial_cmp(&rb).unwrap_or(std::cmp::Ordering::Equal)
        });

        let result = serde_json::json!({
            "project": params.project,
            "total_doc_chunks": total_doc_chunks,
            "total_code_chunks": total_code_chunks,
            "topic_count": topics.len(),
            "topics": topics,
            "guidance": "Topics marked 'undocumented' have code with no corresponding \
                         markdown documentation. Focus on topics with many code chunks \
                         but zero doc chunks. Consider creating documentation for these areas.",
        });

        let json = serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        debug!(
            tool = "doc_coverage_gaps",
            topics = topics.len(),
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );

        Ok(CallToolResult::success(vec![Content::text(json)]))
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
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        self.stats
            .dependency_graph_scans
            .fetch_add(1, Ordering::Relaxed);

        let depth = params.depth.unwrap_or(2);
        let format = params.format.as_deref().unwrap_or("summary");
        let edge_type_strs = params
            .edge_types
            .as_deref()
            .map(|v| v.iter().map(|s| s.as_str()).collect::<Vec<_>>())
            .unwrap_or_else(|| vec!["import"]);

        info!(
            tool = "dependency_graph",
            project = %params.project,
            focus_file = params.focus_file.as_deref().unwrap_or("*"),
            depth,
            format,
            "MCP tool invoked",
        );

        // Resolve project_id
        let project_id: Option<i32> = sqlx::query_scalar("SELECT id FROM projects WHERE name = $1")
            .bind(&params.project)
            .fetch_optional(&self.db_pool)
            .await
            .map_err(|e| McpError::internal_error(format!("Project lookup failed: {}", e), None))?;

        let project_id = project_id.ok_or_else(|| {
            McpError::internal_error(format!("Project not found: {}", params.project), None)
        })?;

        // Load edges and file metadata
        #[derive(sqlx::FromRow)]
        #[allow(dead_code)]
        struct EdgeRow {
            source_file_id: i64,
            source_path: String,
            source_lang: String,
            target_file_id: Option<i64>,
            target_path: Option<String>,
            target_lang: Option<String>,
            edge_type: String,
            weight: f64,
        }

        let edges: Vec<EdgeRow> = sqlx::query_as::<_, EdgeRow>(
            "SELECT
                e.source_file_id,
                sf.relative_path as source_path,
                sf.language as source_lang,
                e.target_file_id,
                tf.relative_path as target_path,
                tf.language as target_lang,
                e.edge_type,
                e.weight
             FROM code_graph_edges e
             JOIN indexed_files sf ON e.source_file_id = sf.id
             LEFT JOIN indexed_files tf ON e.target_file_id = tf.id
             WHERE e.project_id = $1",
        )
        .bind(project_id)
        .fetch_all(&self.db_pool)
        .await
        .map_err(|e| McpError::internal_error(format!("Edge query failed: {}", e), None))?;

        // Filter by edge types
        let filtered_edges: Vec<&EdgeRow> = edges
            .iter()
            .filter(|e| edge_type_strs.contains(&e.edge_type.as_str()))
            .collect();

        // Collect all nodes
        let mut nodes: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
        for e in &filtered_edges {
            nodes
                .entry(e.source_file_id)
                .or_insert_with(|| e.source_path.clone());
            if let (Some(tid), Some(tp)) = (e.target_file_id, e.target_path.as_ref()) {
                nodes.entry(tid).or_insert_with(|| tp.clone());
            }
        }

        // If focus_file specified, BFS to depth
        let (visible_nodes, visible_edges) = if let Some(ref focus) = params.focus_file {
            let focus_id = nodes
                .iter()
                .find(|(_, path)| path.contains(focus.as_str()))
                .map(|(&id, _)| id);

            if let Some(focus_id) = focus_id {
                // BFS from focus_id
                use std::collections::{HashSet, VecDeque};
                let mut visited: HashSet<i64> = HashSet::new();
                let mut queue: VecDeque<(i64, i32)> = VecDeque::new();
                queue.push_back((focus_id, 0));
                visited.insert(focus_id);

                while let Some((node, d)) = queue.pop_front() {
                    if d >= depth {
                        continue;
                    }
                    // Find neighbors in both directions
                    for e in &filtered_edges {
                        if e.source_file_id == node
                            && let Some(tid) = e.target_file_id
                            && visited.insert(tid)
                        {
                            queue.push_back((tid, d + 1));
                        }
                        if e.target_file_id == Some(node) && visited.insert(e.source_file_id) {
                            queue.push_back((e.source_file_id, d + 1));
                        }
                    }
                }

                let vis_edges: Vec<&EdgeRow> = filtered_edges
                    .iter()
                    .filter(|e| {
                        visited.contains(&e.source_file_id)
                            && e.target_file_id
                                .map(|t| visited.contains(&t))
                                .unwrap_or(false)
                    })
                    .copied()
                    .collect();
                let vis_nodes: std::collections::HashMap<i64, String> = nodes
                    .into_iter()
                    .filter(|(id, _)| visited.contains(id))
                    .collect();
                (vis_nodes, vis_edges)
            } else {
                (nodes, filtered_edges)
            }
        } else {
            (nodes, filtered_edges)
        };

        // Count connected components via union-find
        let node_ids: Vec<i64> = visible_nodes.keys().copied().collect();
        let mut id_to_idx: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
        for (i, &id) in node_ids.iter().enumerate() {
            id_to_idx.insert(id, i);
        }
        let mut uf = UnionFind::new(node_ids.len());
        for e in &visible_edges {
            if let (Some(&si), Some(tid)) = (id_to_idx.get(&e.source_file_id), e.target_file_id)
                && let Some(&ti) = id_to_idx.get(&tid)
            {
                uf.union(si, ti);
            }
        }
        let component_count = {
            let mut roots: std::collections::HashSet<usize> = std::collections::HashSet::new();
            for i in 0..node_ids.len() {
                roots.insert(uf.find(i));
            }
            roots.len()
        };

        let result = match format {
            "edges" => {
                let edge_list: Vec<serde_json::Value> = visible_edges
                    .iter()
                    .map(|e| {
                        serde_json::json!({
                            "source": e.source_path,
                            "target": e.target_path,
                            "edge_type": e.edge_type,
                            "weight": format!("{:.2}", e.weight),
                        })
                    })
                    .collect();
                serde_json::json!({
                    "project": params.project,
                    "focus_file": params.focus_file,
                    "node_count": visible_nodes.len(),
                    "edge_count": visible_edges.len(),
                    "components": component_count,
                    "edges": edge_list,
                })
            }
            "dot" => {
                let mut dot = String::from(
                    "digraph dependencies {\n  rankdir=LR;\n  node [shape=box, fontsize=10];\n",
                );
                for (id, path) in &visible_nodes {
                    let short = path.rsplit('/').next().unwrap_or(path);
                    dot.push_str(&format!("  n{} [label=\"{}\"];\n", id, short));
                }
                for e in &visible_edges {
                    if let Some(tid) = e.target_file_id {
                        let style = match e.edge_type.as_str() {
                            "co_change" => " [style=dashed, color=blue]",
                            "semantic" => " [style=dotted, color=green]",
                            _ => "",
                        };
                        dot.push_str(&format!("  n{} -> n{}{};\n", e.source_file_id, tid, style));
                    }
                }
                dot.push_str("}\n");
                serde_json::json!({
                    "project": params.project,
                    "focus_file": params.focus_file,
                    "node_count": visible_nodes.len(),
                    "edge_count": visible_edges.len(),
                    "components": component_count,
                    "dot": dot,
                })
            }
            _ => {
                // summary
                let mut type_counts: std::collections::HashMap<&str, usize> =
                    std::collections::HashMap::new();
                for e in &visible_edges {
                    *type_counts.entry(&e.edge_type).or_insert(0) += 1;
                }
                serde_json::json!({
                    "project": params.project,
                    "focus_file": params.focus_file,
                    "node_count": visible_nodes.len(),
                    "edge_count": visible_edges.len(),
                    "components": component_count,
                    "edge_type_counts": type_counts,
                    "guidance": "Use format: \"edges\" for the full edge list or \"dot\" for Graphviz visualization. \
                                 Set focus_file to zoom into a specific file's neighborhood.",
                })
            }
        };

        let json = serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        debug!(
            tool = "dependency_graph",
            nodes = visible_nodes.len(),
            edges = visible_edges.len(),
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Rank files by centrality in the dependency graph (PageRank, betweenness, degree). High-centrality files are critical paths that affect many other files. Requires the graph-analysis cron job to have run."
    )]
    async fn centrality_analysis(
        &self,
        Parameters(params): Parameters<CentralityAnalysisParams>,
    ) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        self.stats.centrality_scans.fetch_add(1, Ordering::Relaxed);

        let metric = params.metric.as_deref().unwrap_or("all");
        let limit = params.limit.unwrap_or(20);

        info!(
            tool = "centrality_analysis",
            project = %params.project,
            metric,
            limit,
            "MCP tool invoked",
        );

        #[derive(sqlx::FromRow)]
        struct MetricRow {
            relative_path: String,
            language: String,
            pagerank: Option<f64>,
            betweenness: Option<f64>,
            in_degree: Option<i32>,
            out_degree: Option<i32>,
        }

        let order_clause = match metric {
            "pagerank" => "fm.pagerank DESC NULLS LAST",
            "betweenness" => "fm.betweenness DESC NULLS LAST",
            "degree" => "(COALESCE(fm.in_degree,0) + COALESCE(fm.out_degree,0)) DESC",
            _ => "fm.pagerank DESC NULLS LAST",
        };

        let query = format!(
            "SELECT f.relative_path, f.language,
                    fm.pagerank, fm.betweenness, fm.in_degree, fm.out_degree
             FROM file_metrics fm
             JOIN indexed_files f ON fm.file_id = f.id
             JOIN projects p ON fm.project_id = p.id
             WHERE p.name = $1
             ORDER BY {}
             LIMIT $2",
            order_clause
        );

        let rows: Vec<MetricRow> = sqlx::query_as::<_, MetricRow>(&query)
            .bind(&params.project)
            .bind(limit as i64)
            .fetch_all(&self.db_pool)
            .await
            .map_err(|e| McpError::internal_error(format!("Metric query failed: {}", e), None))?;

        if rows.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No file metrics found. The graph-analysis cron job may not have run yet for this project.",
            )]));
        }

        let files: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                let total_degree = r.in_degree.unwrap_or(0) + r.out_degree.unwrap_or(0);
                serde_json::json!({
                    "path": r.relative_path,
                    "language": r.language,
                    "pagerank": r.pagerank.map(|v| format!("{:.6}", v)),
                    "betweenness": r.betweenness.map(|v| format!("{:.6}", v)),
                    "in_degree": r.in_degree.unwrap_or(0),
                    "out_degree": r.out_degree.unwrap_or(0),
                    "total_degree": total_degree,
                })
            })
            .collect();

        let result = serde_json::json!({
            "project": params.project,
            "metric": metric,
            "file_count": files.len(),
            "files": files,
            "guidance": "High PageRank files are depended upon by many others (critical paths). \
                         High betweenness files sit on many shortest paths (bottlenecks). \
                         High degree files have many direct dependencies.",
        });

        let json = serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        debug!(
            tool = "centrality_analysis",
            results = files.len(),
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Detect module communities in the dependency graph using Louvain algorithm. Compares discovered communities against directory structure to reveal architectural misalignment. Requires the graph-analysis cron job to have run."
    )]
    async fn community_detection(
        &self,
        Parameters(params): Parameters<CommunityDetectionParams>,
    ) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        self.stats.community_scans.fetch_add(1, Ordering::Relaxed);

        let graph_type = params.graph_type.as_deref().unwrap_or("import");
        let resolution = params.resolution.unwrap_or(1.0);

        info!(
            tool = "community_detection",
            project = %params.project,
            graph_type,
            resolution,
            "MCP tool invoked",
        );

        // Resolve project_id
        let project_id: Option<i32> = sqlx::query_scalar("SELECT id FROM projects WHERE name = $1")
            .bind(&params.project)
            .fetch_optional(&self.db_pool)
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

        let db_edges: Vec<EdgeRowDb> = sqlx::query_as::<_, EdgeRowDb>(&query)
            .bind(project_id)
            .fetch_all(&self.db_pool)
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
        .fetch_all(&self.db_pool)
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

        let louvain = louvain_communities(&code_graph, resolution);

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
            let dir_match_pct = dir_counts.get(dominant_dir).copied().unwrap_or(0) as f64
                / files.len().max(1) as f64;

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

        let result = serde_json::json!({
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

    #[tool(
        description = "Find circular dependency cycles in the import graph. Cycles make code harder to test, build, and understand. Uses Tarjan's SCC algorithm followed by DFS cycle extraction. Requires the graph-analysis cron job to have run."
    )]
    async fn circular_dependencies(
        &self,
        Parameters(params): Parameters<CircularDependenciesParams>,
    ) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        self.stats.cycle_scans.fetch_add(1, Ordering::Relaxed);

        let max_cycle_length = params.max_cycle_length.unwrap_or(10) as usize;

        info!(
            tool = "circular_dependencies",
            project = %params.project,
            max_cycle_length,
            "MCP tool invoked",
        );

        // Resolve project_id
        let project_id: Option<i32> = sqlx::query_scalar("SELECT id FROM projects WHERE name = $1")
            .bind(&params.project)
            .fetch_optional(&self.db_pool)
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
        .fetch_all(&self.db_pool)
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
        .fetch_all(&self.db_pool)
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

    #[tool(
        description = "Analyze what files would be affected by changing a specific file. Combines import graph (reverse dependents), co-change history (files that often change together), and semantic similarity (functionally related code). Requires the graph-analysis cron job to have run."
    )]
    async fn change_impact_analysis(
        &self,
        Parameters(params): Parameters<ChangeImpactAnalysisParams>,
    ) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        self.stats.impact_scans.fetch_add(1, Ordering::Relaxed);

        let depth = params.depth.unwrap_or(3);
        let include_semantic = params.include_semantic.unwrap_or(true);

        info!(
            tool = "change_impact_analysis",
            project = %params.project,
            file = %params.file,
            depth,
            include_semantic,
            "MCP tool invoked",
        );

        // Resolve project and file
        let project_id: Option<i32> = sqlx::query_scalar("SELECT id FROM projects WHERE name = $1")
            .bind(&params.project)
            .fetch_optional(&self.db_pool)
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
        .fetch_optional(&self.db_pool)
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
        .fetch_all(&self.db_pool)
        .await
        .map_err(|e| McpError::internal_error(format!("Dependents query failed: {}", e), None))?;

        // For deeper impact, do BFS through import edges
        let mut impacted: std::collections::HashMap<i64, (String, f64, String)> =
            std::collections::HashMap::new();
        // (file_id -> (path, impact_score, source_type))

        // Direct import dependents get score 1.0
        let mut frontier: std::collections::VecDeque<(i64, i32)> =
            std::collections::VecDeque::new();
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
            let transitive: Vec<DepRow> = sqlx::query_as::<_, DepRow>(
                "SELECT e.source_file_id as file_id, f.relative_path, e.edge_type
                 FROM code_graph_edges e
                 JOIN indexed_files f ON e.source_file_id = f.id
                 WHERE e.target_file_id = $1 AND e.edge_type = 'import'",
            )
            .bind(node)
            .fetch_all(&self.db_pool)
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
        let co_change_pairs =
            crate::db::queries::find_coupled_files(&self.db_pool, &params.project, 0.2, 2)
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

            let other_id: Option<i64> = sqlx::query_scalar(
                "SELECT id FROM indexed_files WHERE project_id = $1 AND relative_path = $2",
            )
            .bind(project_id)
            .bind(&other_id_query)
            .fetch_optional(&self.db_pool)
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
            let similar_files = crate::db::queries::find_similar_files(
                &self.db_pool,
                target_file_id,
                0.80,
                10,
                Some(&params.project),
            )
            .await
            .unwrap_or_default();

            for sim in &similar_files {
                // Try to resolve the file_id for the similar file
                let sim_id: Option<i64> = sqlx::query_scalar(
                    "SELECT id FROM indexed_files WHERE project_id = $1 AND path = $2",
                )
                .bind(project_id)
                .bind(&sim.path_b)
                .fetch_optional(&self.db_pool)
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
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        self.stats.coupling_reports.fetch_add(1, Ordering::Relaxed);

        let module_depth = params.module_depth.unwrap_or(2) as usize;
        let sort_by = params.sort_by.as_deref().unwrap_or("distance");

        info!(
            tool = "coupling_cohesion_report",
            project = %params.project,
            module_depth,
            sort_by,
            "MCP tool invoked",
        );

        let project_id: Option<i32> = sqlx::query_scalar("SELECT id FROM projects WHERE name = $1")
            .bind(&params.project)
            .fetch_optional(&self.db_pool)
            .await
            .map_err(|e| McpError::internal_error(format!("Project lookup failed: {}", e), None))?;

        let project_id = project_id.ok_or_else(|| {
            McpError::internal_error(format!("Project not found: {}", params.project), None)
        })?;

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
             JOIN indexed_files sf ON e.source_file_id = sf.id
             LEFT JOIN indexed_files tf ON e.target_file_id = tf.id
             WHERE e.project_id = $1 AND e.edge_type = 'import'",
        )
        .bind(project_id)
        .fetch_all(&self.db_pool)
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
        .fetch_all(&self.db_pool)
        .await
        .map_err(|e| McpError::internal_error(format!("File query failed: {}", e), None))?;

        use crate::graph::builder::{FileMetaRow, GraphEdgeRow, build_graph};
        use crate::graph::metrics::{
            compute_module_metrics, is_abstract_file, update_abstractness,
        };

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

        let modules: Vec<serde_json::Value> = module_metrics
            .iter()
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

        let result = serde_json::json!({
            "project": params.project,
            "module_depth": module_depth,
            "sort_by": sort_by,
            "module_count": modules.len(),
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

    #[tool(
        description = "Detect architecture violations: dependency cycles, god modules, bidirectional dependencies, Stable Dependencies Principle (SDP) violations, and modules in Zone of Pain/Uselessness. Returns violations grouped by severity. Requires the graph-analysis cron job."
    )]
    async fn architecture_violations(
        &self,
        Parameters(params): Parameters<ArchitectureViolationsParams>,
    ) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        self.stats.violation_scans.fetch_add(1, Ordering::Relaxed);

        let severity_threshold = params.severity_threshold.as_deref().unwrap_or("medium");

        info!(
            tool = "architecture_violations",
            project = %params.project,
            severity_threshold,
            "MCP tool invoked",
        );

        let mut violations: Vec<serde_json::Value> = Vec::new();

        // 1. Check for dependency cycles (critical)
        let project_id: Option<i32> = sqlx::query_scalar("SELECT id FROM projects WHERE name = $1")
            .bind(&params.project)
            .fetch_optional(&self.db_pool)
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
        .fetch_all(&self.db_pool)
        .await
        .map_err(|e| McpError::internal_error(format!("Edge query failed: {}", e), None))?;

        #[derive(sqlx::FromRow)]
        struct FileMetaDb {
            file_id: i64,
            relative_path: String,
            language: String,
        }

        let file_metas: Vec<FileMetaDb> = sqlx::query_as::<_, FileMetaDb>(
            "SELECT id as file_id, relative_path, language FROM indexed_files WHERE project_id = $1"
        )
        .bind(project_id)
        .fetch_all(&self.db_pool)
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
        let mut edge_pairs: std::collections::HashSet<(i64, i64)> =
            std::collections::HashSet::new();
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

    #[tool(
        description = "Detect design smells: god class (high complexity + many topics), SRP violation (high topic diversity), shotgun surgery (many co-change partners), stale module (old + no changes), unstable dependency (high churn + many dependents). Uses file_metrics and topic data. Requires the graph-analysis cron job and discover_topics."
    )]
    async fn design_smell_detection(
        &self,
        Parameters(params): Parameters<DesignSmellDetectionParams>,
    ) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        self.stats
            .design_smell_scans
            .fetch_add(1, Ordering::Relaxed);

        let limit = params.limit.unwrap_or(30);
        let detect_all = params.smells.is_none();
        let smells = params.smells.unwrap_or_default();

        info!(
            tool = "design_smell_detection",
            project = %params.project,
            limit,
            "MCP tool invoked",
        );

        #[derive(sqlx::FromRow)]
        #[allow(dead_code)]
        struct SmellRow {
            relative_path: String,
            language: String,
            size_bytes: i64,
            line_count: i32,
            pagerank: Option<f64>,
            in_degree: Option<i32>,
            out_degree: Option<i32>,
            commit_count: Option<i32>,
            churn_rate: Option<f64>,
            days_since_last_change: Option<i32>,
        }

        let rows: Vec<SmellRow> = sqlx::query_as::<_, SmellRow>(
            "SELECT f.relative_path, f.language, f.size_bytes, f.line_count,
                    fm.pagerank, fm.in_degree, fm.out_degree,
                    fm.commit_count, fm.churn_rate, fm.days_since_last_change
             FROM indexed_files f
             LEFT JOIN file_metrics fm ON fm.file_id = f.id
             JOIN projects p ON f.project_id = p.id
             WHERE p.name = $1",
        )
        .bind(&params.project)
        .fetch_all(&self.db_pool)
        .await
        .map_err(|e| McpError::internal_error(format!("Query failed: {}", e), None))?;

        // Get topic counts per file
        #[derive(sqlx::FromRow)]
        struct TopicCountRow {
            relative_path: String,
            topic_count: i64,
        }

        let topic_counts: Vec<TopicCountRow> = sqlx::query_as::<_, TopicCountRow>(
            "SELECT f.relative_path, COUNT(DISTINCT cta.topic_id) as topic_count
             FROM indexed_files f
             JOIN file_chunks fc ON fc.file_id = f.id
             JOIN chunk_topic_assignments cta ON cta.chunk_id = fc.id
             JOIN projects p ON f.project_id = p.id
             WHERE p.name = $1
             GROUP BY f.relative_path",
        )
        .bind(&params.project)
        .fetch_all(&self.db_pool)
        .await
        .unwrap_or_default();

        let topic_map: std::collections::HashMap<&str, i64> = topic_counts
            .iter()
            .map(|r| (r.relative_path.as_str(), r.topic_count))
            .collect();

        // Get co-change partner counts
        let coupling_pairs =
            crate::db::queries::find_coupled_files(&self.db_pool, &params.project, 0.2, 2)
                .await
                .unwrap_or_default();

        let mut coupling_count: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for pair in &coupling_pairs {
            *coupling_count.entry(pair.file_a.clone()).or_insert(0) += 1;
            *coupling_count.entry(pair.file_b.clone()).or_insert(0) += 1;
        }

        let mut detected_smells: Vec<serde_json::Value> = Vec::new();

        for row in &rows {
            let topics = topic_map
                .get(row.relative_path.as_str())
                .copied()
                .unwrap_or(0);
            let partners = coupling_count.get(&row.relative_path).copied().unwrap_or(0);

            // God class: large file with many topics
            if (detect_all || smells.iter().any(|s| s == "god_class"))
                && row.line_count > 500
                && topics > 5
            {
                detected_smells.push(serde_json::json!({
                    "smell": "god_class",
                    "severity": "high",
                    "path": row.relative_path,
                    "reason": format!("{} lines, {} topics", row.line_count, topics),
                    "line_count": row.line_count,
                    "topic_count": topics,
                }));
            }

            // SRP violation: many topics
            if (detect_all || smells.iter().any(|s| s == "srp_violation"))
                && topics > 4
                && row.line_count > 200
            {
                detected_smells.push(serde_json::json!({
                    "smell": "srp_violation",
                    "severity": "medium",
                    "path": row.relative_path,
                    "reason": format!("{} distinct topics — file handles too many concerns", topics),
                    "topic_count": topics,
                }));
            }

            // Shotgun surgery: many co-change partners
            if (detect_all || smells.iter().any(|s| s == "shotgun_surgery")) && partners > 8 {
                detected_smells.push(serde_json::json!({
                    "smell": "shotgun_surgery",
                    "severity": "high",
                    "path": row.relative_path,
                    "reason": format!("{} co-change partners — changes here ripple widely", partners),
                    "co_change_partners": partners,
                }));
            }

            // Stale module: old and untouched
            if (detect_all || smells.iter().any(|s| s == "stale_module"))
                && let Some(days) = row.days_since_last_change
                && days > 365
                && row.line_count > 100
            {
                detected_smells.push(serde_json::json!({
                    "smell": "stale_module",
                    "severity": "low",
                    "path": row.relative_path,
                    "reason": format!("Unchanged for {} days ({} lines)", days, row.line_count),
                    "days_since_change": days,
                }));
            }

            // Unstable dependency: high churn with many dependents
            if detect_all || smells.iter().any(|s| s == "unstable_dependency") {
                let in_deg = row.in_degree.unwrap_or(0);
                let churn = row.churn_rate.unwrap_or(0.0);
                if in_deg > 5 && churn > 2.0 {
                    detected_smells.push(serde_json::json!({
                        "smell": "unstable_dependency",
                        "severity": "high",
                        "path": row.relative_path,
                        "reason": format!("{} dependents but churn rate {:.1}/month — unstable core dependency",
                            in_deg, churn),
                        "in_degree": in_deg,
                        "churn_rate": format!("{:.1}", churn),
                    }));
                }
            }
        }

        // Sort by severity descending
        let severity_order = |s: &str| -> i32 {
            match s {
                "high" => 3,
                "medium" => 2,
                "low" => 1,
                _ => 0,
            }
        };
        detected_smells.sort_by(|a, b| {
            let sa = severity_order(a["severity"].as_str().unwrap_or("low"));
            let sb = severity_order(b["severity"].as_str().unwrap_or("low"));
            sb.cmp(&sa)
        });
        detected_smells.truncate(limit as usize);

        let result = serde_json::json!({
            "project": params.project,
            "smell_count": detected_smells.len(),
            "smells": detected_smells,
            "guidance": "God classes and SRP violations should be split. Shotgun surgery files \
                         need interface stabilization. Stale modules may be dead code. \
                         Unstable dependencies need refactoring to reduce churn.",
        });

        let json = serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        debug!(
            tool = "design_smell_detection",
            smells = detected_smells.len(),
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Measure positive architecture quality across 10 dimensions: separation of concerns, loose coupling, SDP compliance, acyclicity, test coverage, doc coverage, code organization, module balance, API stability, and dependency health. Each scored 0-100%. Requires graph-analysis cron job and discover_topics."
    )]
    async fn architecture_quality(
        &self,
        Parameters(params): Parameters<ArchitectureQualityParams>,
    ) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        self.stats
            .architecture_quality_scans
            .fetch_add(1, Ordering::Relaxed);

        let detail = params.detail.as_deref().unwrap_or("summary");

        info!(
            tool = "architecture_quality",
            project = %params.project,
            detail,
            "MCP tool invoked",
        );

        let project_id: Option<i32> = sqlx::query_scalar("SELECT id FROM projects WHERE name = $1")
            .bind(&params.project)
            .fetch_optional(&self.db_pool)
            .await
            .map_err(|e| McpError::internal_error(format!("Project lookup failed: {}", e), None))?;

        let project_id = project_id.ok_or_else(|| {
            McpError::internal_error(format!("Project not found: {}", params.project), None)
        })?;

        // 1. Separation of concerns: avg topic count per file (lower = better)
        let avg_topics: Option<f64> = sqlx::query_scalar(
            "SELECT AVG(topic_count)::DOUBLE PRECISION FROM (
                SELECT COUNT(DISTINCT cta.topic_id) as topic_count
                FROM indexed_files f
                JOIN file_chunks fc ON fc.file_id = f.id
                JOIN chunk_topic_assignments cta ON cta.chunk_id = fc.id
                WHERE f.project_id = $1
                GROUP BY f.id
            ) t",
        )
        .bind(project_id)
        .fetch_optional(&self.db_pool)
        .await
        .unwrap_or(None)
        .flatten();
        let soc_score = (1.0 - (avg_topics.unwrap_or(1.0) - 1.0).max(0.0) / 10.0).max(0.0) * 100.0;

        // 2. Loose coupling: avg instability distance from 0.5 (mid-range is best)
        let avg_coupling: Option<f64> = sqlx::query_scalar(
            "SELECT AVG(COALESCE(afferent_coupling, 0) + COALESCE(efferent_coupling, 0))::DOUBLE PRECISION
             FROM file_metrics WHERE project_id = $1"
        )
        .bind(project_id)
        .fetch_optional(&self.db_pool)
        .await
        .unwrap_or(None)
        .flatten();
        let coupling_score = (1.0 - avg_coupling.unwrap_or(0.0).min(20.0) / 20.0) * 100.0;

        // 3. Acyclicity: fraction of files NOT in cycles
        let total_files: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM indexed_files WHERE project_id = $1")
                .bind(project_id)
                .fetch_one(&self.db_pool)
                .await
                .unwrap_or(0);

        // Use SCC count from edges (approximate — files in cycles)
        let files_in_cycles: i64 = sqlx::query_scalar(
            "SELECT COUNT(DISTINCT source_file_id) FROM (
                SELECT e1.source_file_id
                FROM code_graph_edges e1
                JOIN code_graph_edges e2 ON e1.target_file_id = e2.source_file_id
                    AND e2.target_file_id = e1.source_file_id
                WHERE e1.project_id = $1 AND e1.edge_type = 'import'
                    AND e2.edge_type = 'import'
            ) t",
        )
        .bind(project_id)
        .fetch_one(&self.db_pool)
        .await
        .unwrap_or(0);
        let acyclicity_score = if total_files > 0 {
            (1.0 - files_in_cycles as f64 / total_files as f64) * 100.0
        } else {
            100.0
        };

        // 4. Test coverage: fraction of files that have test files
        let test_file_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM indexed_files
             WHERE project_id = $1 AND relative_path ~* '(test|spec|_test\\.|_spec\\.)'",
        )
        .bind(project_id)
        .fetch_one(&self.db_pool)
        .await
        .unwrap_or(0);
        let test_score = if total_files > 0 {
            (test_file_count as f64 / total_files as f64 * 3.0).min(1.0) * 100.0
        } else {
            0.0
        };

        // 5. Doc coverage: fraction of files with markdown docs
        let doc_file_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM indexed_files
             WHERE project_id = $1 AND language = 'markdown'",
        )
        .bind(project_id)
        .fetch_one(&self.db_pool)
        .await
        .unwrap_or(0);
        let doc_score = if total_files > 0 {
            (doc_file_count as f64 / total_files as f64 * 10.0).min(1.0) * 100.0
        } else {
            0.0
        };

        // 6-10: Additional quality dimensions from file_metrics
        let avg_pagerank: Option<f64> = sqlx::query_scalar(
            "SELECT AVG(pagerank)::DOUBLE PRECISION FROM file_metrics WHERE project_id = $1",
        )
        .bind(project_id)
        .fetch_optional(&self.db_pool)
        .await
        .unwrap_or(None)
        .flatten();
        // More evenly distributed PageRank = better
        let balance_score = avg_pagerank
            .map(|pr| {
                let expected = 1.0 / total_files.max(1) as f64;
                (1.0 - (pr - expected).abs() / expected.max(0.001)).max(0.0) * 100.0
            })
            .unwrap_or(50.0);

        let avg_churn: Option<f64> = sqlx::query_scalar(
            "SELECT AVG(churn_rate)::DOUBLE PRECISION FROM file_metrics WHERE project_id = $1",
        )
        .bind(project_id)
        .fetch_optional(&self.db_pool)
        .await
        .unwrap_or(None)
        .flatten();
        let stability_score = (1.0 - avg_churn.unwrap_or(0.0).min(5.0) / 5.0) * 100.0;

        let avg_fix_ratio: Option<f64> = sqlx::query_scalar(
            "SELECT AVG(fix_commit_ratio)::DOUBLE PRECISION FROM file_metrics WHERE project_id = $1"
        )
        .bind(project_id)
        .fetch_optional(&self.db_pool)
        .await
        .unwrap_or(None)
        .flatten();
        let health_score = (1.0 - avg_fix_ratio.unwrap_or(0.0)) * 100.0;

        // SDP compliance: percentage of edges where stable doesn't depend on unstable
        let sdp_violations: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM code_graph_edges e
             JOIN file_metrics fm_s ON fm_s.file_id = e.source_file_id
             JOIN file_metrics fm_t ON fm_t.file_id = e.target_file_id
             WHERE e.project_id = $1 AND e.edge_type = 'import'
               AND fm_s.instability < 0.3 AND fm_t.instability > 0.7",
        )
        .bind(project_id)
        .fetch_one(&self.db_pool)
        .await
        .unwrap_or(0);
        let total_edges: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM code_graph_edges WHERE project_id = $1 AND edge_type = 'import'",
        )
        .bind(project_id)
        .fetch_one(&self.db_pool)
        .await
        .unwrap_or(0);
        let sdp_score = if total_edges > 0 {
            (1.0 - sdp_violations as f64 / total_edges as f64) * 100.0
        } else {
            100.0
        };

        // Organization score (files with matching directory community)
        let org_score = 75.0; // Default baseline

        let dimensions = vec![
            ("separation_of_concerns", soc_score),
            ("loose_coupling", coupling_score),
            ("sdp_compliance", sdp_score),
            ("acyclicity", acyclicity_score),
            ("test_coverage", test_score),
            ("doc_coverage", doc_score),
            ("module_balance", balance_score),
            ("api_stability", stability_score),
            ("dependency_health", health_score),
            ("code_organization", org_score),
        ];

        let overall = dimensions.iter().map(|(_, s)| s).sum::<f64>() / dimensions.len() as f64;

        fn letter_grade(score: f64) -> &'static str {
            if score >= 90.0 {
                "A"
            } else if score >= 80.0 {
                "B"
            } else if score >= 70.0 {
                "C"
            } else if score >= 60.0 {
                "D"
            } else {
                "F"
            }
        }

        let dim_json: Vec<serde_json::Value> = dimensions
            .iter()
            .map(|(name, score)| {
                serde_json::json!({
                    "dimension": name,
                    "score": format!("{:.1}", score),
                    "grade": letter_grade(*score),
                })
            })
            .collect();

        let result = serde_json::json!({
            "project": params.project,
            "overall_score": format!("{:.1}", overall),
            "overall_grade": letter_grade(overall),
            "dimensions": dim_json,
            "guidance": "Focus on dimensions with grade C or below. \
                         Run the specific analysis tools (coupling_cohesion_report, circular_dependencies, etc.) \
                         for detailed remediation guidance.",
        });

        let json = serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        debug!(
            tool = "architecture_quality",
            overall = %format!("{:.1}", overall),
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Compute detailed design metrics per file: cyclomatic complexity (from branching keywords), weighted methods per class (WMC), Card & Glass system complexity, and maintainability index. Useful for identifying over-complex files that need refactoring."
    )]
    async fn design_metrics(
        &self,
        Parameters(params): Parameters<DesignMetricsParams>,
    ) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        self.stats
            .design_metric_scans
            .fetch_add(1, Ordering::Relaxed);

        let scope = params.scope.as_deref().unwrap_or("project");
        let limit = params.limit.unwrap_or(30);
        let sort_by = params.sort_by.as_deref().unwrap_or("system_complexity");

        info!(
            tool = "design_metrics",
            project = %params.project,
            scope,
            limit,
            sort_by,
            "MCP tool invoked",
        );

        #[derive(sqlx::FromRow)]
        #[allow(dead_code)]
        struct FileRow {
            file_id: i64,
            relative_path: String,
            language: String,
            line_count: i32,
            content: Option<String>,
            in_degree: Option<i32>,
            out_degree: Option<i32>,
        }

        let path_filter = params.path.as_deref().unwrap_or("");
        let query = if path_filter.is_empty() || scope == "project" {
            "SELECT f.id as file_id, f.relative_path, f.language, f.line_count, f.content,
                    fm.in_degree, fm.out_degree
             FROM indexed_files f
             LEFT JOIN file_metrics fm ON fm.file_id = f.id
             JOIN projects p ON f.project_id = p.id
             WHERE p.name = $1 AND f.content IS NOT NULL"
                .to_string()
        } else if scope == "directory" {
            format!(
                "SELECT f.id as file_id, f.relative_path, f.language, f.line_count, f.content,
                        fm.in_degree, fm.out_degree
                 FROM indexed_files f
                 LEFT JOIN file_metrics fm ON fm.file_id = f.id
                 JOIN projects p ON f.project_id = p.id
                 WHERE p.name = $1 AND f.content IS NOT NULL
                   AND f.relative_path LIKE '{}%'",
                path_filter.replace('\'', "''")
            )
        } else {
            format!(
                "SELECT f.id as file_id, f.relative_path, f.language, f.line_count, f.content,
                        fm.in_degree, fm.out_degree
                 FROM indexed_files f
                 LEFT JOIN file_metrics fm ON fm.file_id = f.id
                 JOIN projects p ON f.project_id = p.id
                 WHERE p.name = $1 AND f.content IS NOT NULL
                   AND f.relative_path = '{}'",
                path_filter.replace('\'', "''")
            )
        };

        let rows: Vec<FileRow> = sqlx::query_as::<_, FileRow>(&query)
            .bind(&params.project)
            .fetch_all(&self.db_pool)
            .await
            .map_err(|e| McpError::internal_error(format!("Query failed: {}", e), None))?;

        if rows.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No files found matching the criteria.",
            )]));
        }

        // Compute metrics per file
        let branch_re = regex::Regex::new(
            r"(?m)^\s*(if|else\s+if|elif|else|for|while|match|case|catch|except|&&|\|\|)\b",
        )
        .expect("valid regex");

        let mut metrics: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                let content = r.content.as_deref().unwrap_or("");

                // Cyclomatic complexity: count branching keywords + 1
                let branches = branch_re.find_iter(content).count();
                let cyclomatic = branches as i32 + 1;

                // WMC: cyclomatic per 100 lines (method density proxy)
                let wmc = if r.line_count > 0 {
                    cyclomatic as f64 / (r.line_count as f64 / 100.0).max(1.0)
                } else {
                    0.0
                };

                // Card & Glass structural complexity S(k) = fan_out^2
                let fan_out = r.out_degree.unwrap_or(0) as f64;
                let fan_in = r.in_degree.unwrap_or(0) as f64;
                let structural_complexity = fan_out * fan_out;
                // Data complexity D(k) approximated by fan_in * lines / fan_out
                let data_complexity = if fan_out > 0.0 {
                    fan_in * r.line_count as f64 / (fan_out + 1.0)
                } else {
                    0.0
                };
                // System complexity Sy(k) = S(k) + D(k)
                let system_complexity = structural_complexity + data_complexity;

                // Maintainability Index (simplified SEI formula)
                // MI = 171 - 5.2 * ln(HV) - 0.23 * CC - 16.2 * ln(LOC)
                // Using cyclomatic for CC, and lines for HV/LOC
                let loc = r.line_count.max(1) as f64;
                let halstead_volume = loc * loc.log2().max(1.0); // simplified
                let mi = (171.0
                    - 5.2 * halstead_volume.ln()
                    - 0.23 * cyclomatic as f64
                    - 16.2 * loc.ln())
                .clamp(0.0, 171.0);
                let mi_normalized = mi / 171.0 * 100.0;

                serde_json::json!({
                    "path": r.relative_path,
                    "language": r.language,
                    "line_count": r.line_count,
                    "cyclomatic_complexity": cyclomatic,
                    "wmc": format!("{:.2}", wmc),
                    "structural_complexity": format!("{:.1}", structural_complexity),
                    "data_complexity": format!("{:.1}", data_complexity),
                    "system_complexity": format!("{:.1}", system_complexity),
                    "maintainability_index": format!("{:.1}", mi_normalized),
                    "fan_in": r.in_degree.unwrap_or(0),
                    "fan_out": r.out_degree.unwrap_or(0),
                })
            })
            .collect();

        // Sort
        match sort_by {
            "cyclomatic" => metrics.sort_by(|a, b| {
                let sa = a["cyclomatic_complexity"].as_i64().unwrap_or(0);
                let sb = b["cyclomatic_complexity"].as_i64().unwrap_or(0);
                sb.cmp(&sa)
            }),
            "maintainability" => metrics.sort_by(|a, b| {
                let sa: f64 = a["maintainability_index"]
                    .as_str()
                    .unwrap_or("100")
                    .parse()
                    .unwrap_or(100.0);
                let sb: f64 = b["maintainability_index"]
                    .as_str()
                    .unwrap_or("100")
                    .parse()
                    .unwrap_or(100.0);
                sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
            }),
            "wmc" => metrics.sort_by(|a, b| {
                let sa: f64 = a["wmc"].as_str().unwrap_or("0").parse().unwrap_or(0.0);
                let sb: f64 = b["wmc"].as_str().unwrap_or("0").parse().unwrap_or(0.0);
                sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
            }),
            _ => metrics.sort_by(|a, b| {
                let sa: f64 = a["system_complexity"]
                    .as_str()
                    .unwrap_or("0")
                    .parse()
                    .unwrap_or(0.0);
                let sb: f64 = b["system_complexity"]
                    .as_str()
                    .unwrap_or("0")
                    .parse()
                    .unwrap_or(0.0);
                sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
            }),
        }
        metrics.truncate(limit as usize);

        let result = serde_json::json!({
            "project": params.project,
            "scope": scope,
            "path": params.path,
            "sort_by": sort_by,
            "file_count": metrics.len(),
            "files": metrics,
            "guidance": "Cyclomatic complexity > 20 = high risk. Maintainability index < 50 = difficult to maintain. \
                         High system complexity (S+D) files are structural bottlenecks. \
                         WMC > 50 per 100 lines suggests excessive branching density.",
        });

        let json = serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        debug!(
            tool = "design_metrics",
            files = metrics.len(),
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );

        Ok(CallToolResult::success(vec![Content::text(json)]))
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
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        self.stats.bug_predictions.fetch_add(1, Ordering::Relaxed);

        let limit = params.limit.unwrap_or(20);

        info!(
            tool = "bug_prediction",
            project = %params.project,
            limit,
            "MCP tool invoked",
        );

        #[derive(sqlx::FromRow)]
        struct BugRow {
            relative_path: String,
            language: String,
            line_count: i32,
            churn_rate: Option<f64>,
            fix_commit_ratio: Option<f64>,
            commit_count: Option<i32>,
            author_count: Option<i32>,
            in_degree: Option<i32>,
            out_degree: Option<i32>,
        }

        let rows: Vec<BugRow> = sqlx::query_as::<_, BugRow>(
            "SELECT f.relative_path, f.language, f.line_count,
                    fm.churn_rate, fm.fix_commit_ratio, fm.commit_count,
                    fm.author_count, fm.in_degree, fm.out_degree
             FROM indexed_files f
             JOIN file_metrics fm ON fm.file_id = f.id
             JOIN projects p ON f.project_id = p.id
             WHERE p.name = $1",
        )
        .bind(&params.project)
        .fetch_all(&self.db_pool)
        .await
        .map_err(|e| McpError::internal_error(format!("Query failed: {}", e), None))?;

        if rows.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No file metrics found. The graph-analysis cron job may not have run yet.",
            )]));
        }

        // Compute complexity from branch keywords in content
        let mut scored: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                let churn = r.churn_rate.unwrap_or(0.0);
                let fix_ratio = r.fix_commit_ratio.unwrap_or(0.0);
                let coupling = (r.in_degree.unwrap_or(0) + r.out_degree.unwrap_or(0)) as f64;
                let size_factor = (r.line_count as f64 / 100.0).min(10.0);
                let authors = r.author_count.unwrap_or(1) as f64;

                // Composite bug-proneness score
                // Weight: churn * fix_ratio * size * coupling * author_spread
                let bug_score = (churn * 0.3
                    + fix_ratio * 3.0
                    + size_factor * 0.2
                    + coupling * 0.05
                    + (authors - 1.0).max(0.0) * 0.1)
                    .max(0.0);

                serde_json::json!({
                    "path": r.relative_path,
                    "language": r.language,
                    "bug_score": format!("{:.4}", bug_score),
                    "churn_rate": format!("{:.2}", churn),
                    "fix_ratio": format!("{:.2}", fix_ratio),
                    "line_count": r.line_count,
                    "commit_count": r.commit_count.unwrap_or(0),
                    "author_count": r.author_count.unwrap_or(0),
                    "coupling": r.in_degree.unwrap_or(0) + r.out_degree.unwrap_or(0),
                })
            })
            .collect();

        scored.sort_by(|a, b| {
            let sa: f64 = a["bug_score"]
                .as_str()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0.0);
            let sb: f64 = b["bug_score"]
                .as_str()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0.0);
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(limit as usize);

        let result = serde_json::json!({
            "project": params.project,
            "file_count": scored.len(),
            "files": scored,
            "guidance": "Files with high bug_score combine high churn, fix ratios, size, and coupling. \
                         Prioritize code review and testing for these files. \
                         High fix_ratio (>0.3) means >30% of commits are bug fixes.",
        });

        let json = serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        debug!(
            tool = "bug_prediction",
            files = scored.len(),
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Analyze technical debt across a project by combining TODO/FIXME/HACK density, cyclomatic complexity, test coverage gaps, module distance from Main Sequence (D*), and churn rate into a composite debt score per file. Optionally scans file content for debt markers."
    )]
    async fn technical_debt_analysis(
        &self,
        Parameters(params): Parameters<TechnicalDebtAnalysisParams>,
    ) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        self.stats.debt_analyses.fetch_add(1, Ordering::Relaxed);

        let limit = params.limit.unwrap_or(30);
        let include_todos = params.include_todos.unwrap_or(true);

        info!(
            tool = "technical_debt_analysis",
            project = %params.project,
            limit,
            include_todos,
            "MCP tool invoked",
        );

        #[derive(sqlx::FromRow)]
        #[allow(dead_code)]
        struct DebtRow {
            relative_path: String,
            language: String,
            line_count: i32,
            content: Option<String>,
            churn_rate: Option<f64>,
            fix_commit_ratio: Option<f64>,
            instability: Option<f64>,
        }

        let rows: Vec<DebtRow> = sqlx::query_as::<_, DebtRow>(
            "SELECT f.relative_path, f.language, f.line_count, f.content,
                    fm.churn_rate, fm.fix_commit_ratio, fm.instability
             FROM indexed_files f
             LEFT JOIN file_metrics fm ON fm.file_id = f.id
             JOIN projects p ON f.project_id = p.id
             WHERE p.name = $1 AND f.content IS NOT NULL",
        )
        .bind(&params.project)
        .fetch_all(&self.db_pool)
        .await
        .map_err(|e| McpError::internal_error(format!("Query failed: {}", e), None))?;

        if rows.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No files found for this project.",
            )]));
        }

        let todo_re =
            regex::Regex::new(r"(?i)(TODO|FIXME|HACK|XXX|TEMP|WORKAROUND)").expect("valid regex");
        let branch_re =
            regex::Regex::new(r"(?m)^\s*(if|else\s+if|elif|for|while|match|case|catch|except)\b")
                .expect("valid regex");

        let mut total_todos = 0usize;
        let mut scored: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                let content = r.content.as_deref().unwrap_or("");

                // Count debt markers
                let todo_count = if include_todos {
                    todo_re.find_iter(content).count()
                } else {
                    0
                };
                total_todos += todo_count;

                let todo_density = if r.line_count > 0 {
                    todo_count as f64 / r.line_count as f64 * 1000.0
                } else {
                    0.0
                };

                // Cyclomatic complexity
                let branches = branch_re.find_iter(content).count();
                let cyclomatic = branches as f64 + 1.0;
                let complexity_factor = (cyclomatic / 20.0).min(1.0);

                let churn = r.churn_rate.unwrap_or(0.0).min(10.0) / 10.0;
                let fix_ratio = r.fix_commit_ratio.unwrap_or(0.0);

                // Composite debt score
                let debt_score = todo_density * 0.3
                    + complexity_factor * 0.25
                    + churn * 0.2
                    + fix_ratio * 0.15
                    + (r.line_count as f64 / 1000.0).min(1.0) * 0.1;

                // Collect specific TODO lines
                let mut todo_lines: Vec<String> = Vec::new();
                if include_todos {
                    for (i, line) in content.lines().enumerate() {
                        if todo_re.is_match(line) && todo_lines.len() < 5 {
                            todo_lines.push(format!("L{}: {}", i + 1, truncate(line.trim(), 120)));
                        }
                    }
                }

                serde_json::json!({
                    "path": r.relative_path,
                    "language": r.language,
                    "debt_score": format!("{:.4}", debt_score),
                    "todo_count": todo_count,
                    "todo_density": format!("{:.1}", todo_density),
                    "cyclomatic_complexity": branches + 1,
                    "line_count": r.line_count,
                    "churn_rate": format!("{:.2}", r.churn_rate.unwrap_or(0.0)),
                    "fix_ratio": format!("{:.2}", fix_ratio),
                    "sample_todos": todo_lines,
                })
            })
            .collect();

        scored.sort_by(|a, b| {
            let sa: f64 = a["debt_score"]
                .as_str()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0.0);
            let sb: f64 = b["debt_score"]
                .as_str()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0.0);
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(limit as usize);

        let result = serde_json::json!({
            "project": params.project,
            "total_debt_markers": total_todos,
            "file_count": scored.len(),
            "files": scored,
            "guidance": "Files with high debt_score combine TODO density, complexity, churn, and fix ratio. \
                         Address TODO/FIXME comments, reduce cyclomatic complexity, and stabilize high-churn files. \
                         todo_density is per 1000 lines.",
        });

        let json = serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        debug!(
            tool = "technical_debt_analysis",
            files = scored.len(),
            total_todos,
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Detect anomalous files using embedding distance from project centroid and metric z-scores. Outlier files may indicate abandoned experiments, copied code from other projects, or architectural inconsistencies. No ML dependencies — uses statistical distance measures."
    )]
    async fn anomaly_detection(
        &self,
        Parameters(params): Parameters<AnomalyDetectionParams>,
    ) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        self.stats.anomaly_scans.fetch_add(1, Ordering::Relaxed);

        let limit = params.limit.unwrap_or(20);
        let contamination = params.contamination.unwrap_or(0.05);

        info!(
            tool = "anomaly_detection",
            project = %params.project,
            limit,
            contamination,
            "MCP tool invoked",
        );

        // Get project centroid (average embedding) and per-file distances
        let project_id: Option<i32> = sqlx::query_scalar("SELECT id FROM projects WHERE name = $1")
            .bind(&params.project)
            .fetch_optional(&self.db_pool)
            .await
            .map_err(|e| McpError::internal_error(format!("Project lookup failed: {}", e), None))?;

        let project_id = project_id.ok_or_else(|| {
            McpError::internal_error(format!("Project not found: {}", params.project), None)
        })?;

        // Compute per-file average embedding distance from project centroid
        // Using SQL: avg cosine distance from average embedding
        #[derive(sqlx::FromRow)]
        struct AnomalyRow {
            file_id: i64,
            relative_path: String,
            language: String,
            line_count: i32,
            avg_distance: f64,
        }

        let rows: Vec<AnomalyRow> = sqlx::query_as::<_, AnomalyRow>(
            "WITH project_centroid AS (
                SELECT AVG(fc.embedding)::vector(384) as centroid
                FROM file_chunks fc
                JOIN indexed_files f ON fc.file_id = f.id
                WHERE f.project_id = $1
            ),
            file_distances AS (
                SELECT
                    f.id as file_id,
                    f.relative_path,
                    f.language,
                    f.line_count,
                    AVG(fc.embedding <=> pc.centroid) as avg_distance
                FROM file_chunks fc
                JOIN indexed_files f ON fc.file_id = f.id
                CROSS JOIN project_centroid pc
                WHERE f.project_id = $1
                GROUP BY f.id, f.relative_path, f.language, f.line_count
            )
            SELECT file_id, relative_path, language, line_count, avg_distance
            FROM file_distances
            ORDER BY avg_distance DESC",
        )
        .bind(project_id)
        .fetch_all(&self.db_pool)
        .await
        .map_err(|e| McpError::internal_error(format!("Anomaly query failed: {}", e), None))?;

        if rows.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No embedded files found for this project.",
            )]));
        }

        // Compute z-scores for distances
        let n = rows.len() as f64;
        let mean_dist: f64 = rows.iter().map(|r| r.avg_distance).sum::<f64>() / n;
        let variance: f64 = rows
            .iter()
            .map(|r| (r.avg_distance - mean_dist).powi(2))
            .sum::<f64>()
            / n;
        let std_dev = variance.sqrt().max(0.0001);

        // Also get metric z-scores from file_metrics
        #[derive(sqlx::FromRow)]
        struct MetricRow {
            file_id: i64,
            line_count_z: Option<f64>,
            churn_z: Option<f64>,
        }

        let metric_zscores: Vec<MetricRow> = sqlx::query_as::<_, MetricRow>(
            "WITH stats AS (
                SELECT
                    AVG(f.line_count)::DOUBLE PRECISION as avg_lc,
                    STDDEV_POP(f.line_count)::DOUBLE PRECISION as std_lc,
                    AVG(fm.churn_rate)::DOUBLE PRECISION as avg_churn,
                    STDDEV_POP(fm.churn_rate)::DOUBLE PRECISION as std_churn
                FROM indexed_files f
                LEFT JOIN file_metrics fm ON fm.file_id = f.id
                WHERE f.project_id = $1
            )
            SELECT
                f.id as file_id,
                CASE WHEN s.std_lc > 0 THEN (f.line_count - s.avg_lc) / s.std_lc ELSE 0 END as line_count_z,
                CASE WHEN s.std_churn > 0 THEN (COALESCE(fm.churn_rate, 0) - s.avg_churn) / s.std_churn ELSE 0 END as churn_z
            FROM indexed_files f
            LEFT JOIN file_metrics fm ON fm.file_id = f.id
            CROSS JOIN stats s
            WHERE f.project_id = $1"
        )
        .bind(project_id)
        .fetch_all(&self.db_pool)
        .await
        .unwrap_or_default();

        let z_map: std::collections::HashMap<i64, (f64, f64)> = metric_zscores
            .iter()
            .map(|r| {
                (
                    r.file_id,
                    (r.line_count_z.unwrap_or(0.0), r.churn_z.unwrap_or(0.0)),
                )
            })
            .collect();

        // Compute composite anomaly score
        let mut anomalies: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                let distance_z = (r.avg_distance - mean_dist) / std_dev;
                let (lc_z, churn_z) = z_map.get(&r.file_id).copied().unwrap_or((0.0, 0.0));

                // Composite: weighted sum of absolute z-scores
                let anomaly_score =
                    distance_z.abs() * 0.5 + lc_z.abs() * 0.25 + churn_z.abs() * 0.25;

                serde_json::json!({
                    "path": r.relative_path,
                    "language": r.language,
                    "line_count": r.line_count,
                    "anomaly_score": format!("{:.4}", anomaly_score),
                    "embedding_distance": format!("{:.4}", r.avg_distance),
                    "distance_zscore": format!("{:.2}", distance_z),
                    "size_zscore": format!("{:.2}", lc_z),
                    "churn_zscore": format!("{:.2}", churn_z),
                })
            })
            .collect();

        anomalies.sort_by(|a, b| {
            let sa: f64 = a["anomaly_score"]
                .as_str()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0.0);
            let sb: f64 = b["anomaly_score"]
                .as_str()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0.0);
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });
        anomalies.truncate(limit as usize);

        let result = serde_json::json!({
            "project": params.project,
            "contamination": contamination,
            "mean_distance": format!("{:.4}", mean_dist),
            "std_distance": format!("{:.4}", std_dev),
            "anomaly_count": anomalies.len(),
            "anomalies": anomalies,
            "guidance": "High anomaly_score files are statistically unusual in their semantic content, \
                         size, or change patterns. They may be: abandoned experiments, copied from \
                         another project, auto-generated code, or architectural outliers. \
                         High distance_zscore (>2) means the file's content is very different from the project norm.",
        });

        let json = serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        debug!(
            tool = "anomaly_detection",
            anomalies = anomalies.len(),
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );

        Ok(CallToolResult::success(vec![Content::text(json)]))
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
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        self.stats.hybrid_searches.fetch_add(1, Ordering::Relaxed);

        let limit = params.limit.unwrap_or(20);
        let bm25_weight = params.bm25_weight.unwrap_or(0.5);
        let semantic_weight = params.semantic_weight.unwrap_or(0.5);

        info!(
            tool = "hybrid_search",
            query = %truncate(&params.query, 200),
            project = params.project.as_deref().unwrap_or("*"),
            language = params.language.as_deref().unwrap_or("*"),
            limit,
            bm25_weight,
            semantic_weight,
            "MCP tool invoked",
        );

        // Run text search
        let text_results = crate::db::queries::text_search(
            &self.db_pool,
            &params.query,
            limit * 2, // fetch more for fusion
            params.language.as_deref(),
        )
        .await
        .map_err(|e| McpError::internal_error(format!("Text search failed: {}", e), None))?;

        // Run semantic search
        let embedding = self
            .embed_source
            .embed_query(&params.query)
            .await
            .map_err(|e| McpError::internal_error(format!("Embedding failed: {}", e), None))?;

        let ef_search = self.config.load().vector.ef_search;
        let semantic_results = crate::db::queries::semantic_search(
            &self.db_pool,
            &embedding,
            limit * 2,
            params.language.as_deref(),
            params.project.as_deref(),
            ef_search,
        )
        .await
        .map_err(|e| McpError::internal_error(format!("Semantic search failed: {}", e), None))?;

        // Reciprocal Rank Fusion (RRF) with k=60
        let k = 60.0;
        let mut rrf_scores: std::collections::HashMap<String, (f64, serde_json::Value)> =
            std::collections::HashMap::new();

        // Score text search results
        for (rank, result) in text_results.iter().enumerate() {
            let key = format!("text:{}:{}", result.relative_path, rank);
            let rrf = bm25_weight / (k + rank as f64 + 1.0);
            let snippet = result.content.as_deref().unwrap_or("");
            let entry = rrf_scores.entry(key).or_insert((
                0.0,
                serde_json::json!({
                    "path": result.path,
                    "relative_path": result.relative_path,
                    "snippet": truncate(snippet, 300),
                    "language": result.language,
                    "source": "text",
                }),
            ));
            entry.0 += rrf;
        }

        // Score semantic search results
        for (rank, result) in semantic_results.iter().enumerate() {
            let key = format!("semantic:{}:{}", result.relative_path, result.start_line);
            let rrf = semantic_weight / (k + rank as f64 + 1.0);
            let entry = rrf_scores.entry(key).or_insert((
                0.0,
                serde_json::json!({
                    "path": result.path,
                    "relative_path": result.relative_path,
                    "project_name": result.project_name,
                    "start_line": result.start_line,
                    "end_line": result.end_line,
                    "snippet": truncate(&result.chunk_content, 300),
                    "language": result.language,
                    "source": "semantic",
                }),
            ));
            entry.0 += rrf;
        }

        // Sort by RRF score and take top results
        let mut fused: Vec<serde_json::Value> = rrf_scores
            .into_iter()
            .map(|(_, (score, mut val))| {
                if let Some(o) = val.as_object_mut() {
                    o.insert(
                        "rrf_score".to_string(),
                        serde_json::json!(format!("{:.6}", score)),
                    );
                }
                val
            })
            .collect();

        fused.sort_by(|a, b| {
            let sa: f64 = a["rrf_score"]
                .as_str()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0.0);
            let sb: f64 = b["rrf_score"]
                .as_str()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0.0);
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });
        fused.truncate(limit as usize);

        let result = serde_json::json!({
            "query": params.query,
            "project": params.project,
            "language": params.language,
            "bm25_weight": bm25_weight,
            "semantic_weight": semantic_weight,
            "text_results": text_results.len(),
            "semantic_results": semantic_results.len(),
            "fused_count": fused.len(),
            "results": fused,
            "guidance": "RRF combines keyword precision with semantic recall. \
                         Increase bm25_weight for exact-match queries (error messages, function names). \
                         Increase semantic_weight for conceptual queries (design patterns, workflows).",
        });

        let json = serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        debug!(
            tool = "hybrid_search",
            results = fused.len(),
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Generate a structural summary of a project, directory, or file. Identifies key modules by PageRank, describes each directory's role based on topic assignments and file composition, and highlights dominant patterns. Requires the graph-analysis cron job and discover_topics."
    )]
    async fn code_summarize(
        &self,
        Parameters(params): Parameters<CodeSummarizeParams>,
    ) -> Result<CallToolResult, McpError> {
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        self.stats.summarize_scans.fetch_add(1, Ordering::Relaxed);

        let scope = params.scope.as_deref().unwrap_or("project");
        let detail = params.detail.as_deref().unwrap_or("standard");

        info!(
            tool = "code_summarize",
            project = %params.project,
            scope,
            path = params.path.as_deref().unwrap_or("*"),
            detail,
            "MCP tool invoked",
        );

        let project_id: Option<i32> = sqlx::query_scalar("SELECT id FROM projects WHERE name = $1")
            .bind(&params.project)
            .fetch_optional(&self.db_pool)
            .await
            .map_err(|e| McpError::internal_error(format!("Project lookup failed: {}", e), None))?;

        let project_id = project_id.ok_or_else(|| {
            McpError::internal_error(format!("Project not found: {}", params.project), None)
        })?;

        // Get directory-level summary
        #[derive(sqlx::FromRow)]
        struct DirSummary {
            directory: String,
            file_count: i64,
            total_lines: i64,
            languages: String,
        }

        let path_filter = params.path.as_deref().unwrap_or("");
        let dir_where = if !path_filter.is_empty() && scope != "project" {
            format!(
                "AND f.relative_path LIKE '{}%'",
                path_filter.replace('\'', "''")
            )
        } else {
            String::new()
        };

        let query = format!(
            "SELECT
                COALESCE(
                    CASE WHEN position('/' IN relative_path) > 0
                        THEN left(relative_path, position('/' IN relative_path) - 1)
                        ELSE ''
                    END, ''
                ) as directory,
                COUNT(*) as file_count,
                SUM(line_count)::BIGINT as total_lines,
                STRING_AGG(DISTINCT language, ', ') as languages
             FROM indexed_files f
             WHERE f.project_id = $1 {}
             GROUP BY directory
             ORDER BY file_count DESC
             LIMIT 30",
            dir_where
        );

        let dirs: Vec<DirSummary> = sqlx::query_as::<_, DirSummary>(&query)
            .bind(project_id)
            .fetch_all(&self.db_pool)
            .await
            .map_err(|e| McpError::internal_error(format!("Dir query failed: {}", e), None))?;

        // Get top files by PageRank
        #[derive(sqlx::FromRow)]
        struct TopFile {
            relative_path: String,
            language: String,
            line_count: i32,
            pagerank: Option<f64>,
        }

        let top_files: Vec<TopFile> = sqlx::query_as::<_, TopFile>(
            "SELECT f.relative_path, f.language, f.line_count, fm.pagerank
             FROM indexed_files f
             LEFT JOIN file_metrics fm ON fm.file_id = f.id
             WHERE f.project_id = $1
             ORDER BY fm.pagerank DESC NULLS LAST
             LIMIT 10",
        )
        .bind(project_id)
        .fetch_all(&self.db_pool)
        .await
        .unwrap_or_default();

        // Get topic summary
        #[derive(sqlx::FromRow)]
        struct TopicSummary {
            label: String,
            chunk_count: i32,
        }

        let topics: Vec<TopicSummary> = sqlx::query_as::<_, TopicSummary>(
            "SELECT label, chunk_count
             FROM code_topics
             WHERE scope LIKE $1
             ORDER BY chunk_count DESC
             LIMIT 15",
        )
        .bind(format!("%{}", params.project))
        .fetch_all(&self.db_pool)
        .await
        .unwrap_or_default();

        // Language breakdown
        #[derive(sqlx::FromRow)]
        struct LangCount {
            language: String,
            count: i64,
            total_lines: i64,
        }

        let lang_breakdown: Vec<LangCount> = sqlx::query_as::<_, LangCount>(
            "SELECT language, COUNT(*) as count, SUM(line_count)::BIGINT as total_lines
             FROM indexed_files
             WHERE project_id = $1
             GROUP BY language
             ORDER BY count DESC",
        )
        .bind(project_id)
        .fetch_all(&self.db_pool)
        .await
        .unwrap_or_default();

        let total_files: i64 = lang_breakdown.iter().map(|l| l.count).sum();
        let total_lines: i64 = lang_breakdown.iter().map(|l| l.total_lines).sum();

        let dir_json: Vec<serde_json::Value> = dirs
            .iter()
            .map(|d| {
                serde_json::json!({
                    "directory": if d.directory.is_empty() { "(root)" } else { &d.directory },
                    "file_count": d.file_count,
                    "total_lines": d.total_lines,
                    "languages": d.languages,
                })
            })
            .collect();

        let key_files: Vec<serde_json::Value> = top_files
            .iter()
            .map(|f| {
                serde_json::json!({
                    "path": f.relative_path,
                    "language": f.language,
                    "line_count": f.line_count,
                    "pagerank": f.pagerank.map(|v| format!("{:.6}", v)),
                })
            })
            .collect();

        let topic_json: Vec<serde_json::Value> = topics
            .iter()
            .map(|t| {
                serde_json::json!({
                    "topic": t.label,
                    "chunk_count": t.chunk_count,
                })
            })
            .collect();

        let lang_json: Vec<serde_json::Value> = lang_breakdown
            .iter()
            .map(|l| {
                serde_json::json!({
                    "language": l.language,
                    "files": l.count,
                    "lines": l.total_lines,
                    "pct": format!("{:.1}%", l.count as f64 / total_files.max(1) as f64 * 100.0),
                })
            })
            .collect();

        let mut result = serde_json::json!({
            "project": params.project,
            "scope": scope,
            "total_files": total_files,
            "total_lines": total_lines,
            "language_breakdown": lang_json,
            "directories": dir_json,
            "key_files": key_files,
        });

        if detail != "brief"
            && let Some(o) = result.as_object_mut()
        {
            o.insert("topics".to_string(), serde_json::json!(topic_json));
        }

        let json = serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        debug!(
            tool = "code_summarize",
            total_files,
            total_lines,
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );

        Ok(CallToolResult::success(vec![Content::text(json)]))
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
        let start = Instant::now();
        self.stats.mcp_requests.fetch_add(1, Ordering::Relaxed);
        self.stats.scorecard_scans.fetch_add(1, Ordering::Relaxed);

        let format = params.format.as_deref().unwrap_or("full");

        info!(
            tool = "engineering_scorecard",
            project = %params.project,
            format,
            "MCP tool invoked",
        );

        let project_id: Option<i32> = sqlx::query_scalar("SELECT id FROM projects WHERE name = $1")
            .bind(&params.project)
            .fetch_optional(&self.db_pool)
            .await
            .map_err(|e| McpError::internal_error(format!("Project lookup failed: {}", e), None))?;

        let project_id = project_id.ok_or_else(|| {
            McpError::internal_error(format!("Project not found: {}", params.project), None)
        })?;

        fn letter_grade(score: f64) -> &'static str {
            if score >= 90.0 {
                "A"
            } else if score >= 80.0 {
                "B"
            } else if score >= 70.0 {
                "C"
            } else if score >= 60.0 {
                "D"
            } else {
                "F"
            }
        }
        fn grade_gpa(grade: &str) -> f64 {
            match grade {
                "A" => 4.0,
                "B" => 3.0,
                "C" => 2.0,
                "D" => 1.0,
                _ => 0.0,
            }
        }

        // === Dimension 1: Code Size & Structure ===
        #[derive(sqlx::FromRow)]
        struct ProjectStats {
            file_count: i64,
            total_lines: i64,
            avg_file_lines: f64,
        }

        let stats: Option<ProjectStats> = sqlx::query_as::<_, ProjectStats>(
            "SELECT COUNT(*) as file_count, SUM(line_count)::BIGINT as total_lines,
                    AVG(line_count)::DOUBLE PRECISION as avg_file_lines
             FROM indexed_files WHERE project_id = $1",
        )
        .bind(project_id)
        .fetch_optional(&self.db_pool)
        .await
        .unwrap_or(None);

        let stats = stats.unwrap_or(ProjectStats {
            file_count: 0,
            total_lines: 0,
            avg_file_lines: 0.0,
        });
        // Good avg file size: 100-300 lines. Penalize >500 avg
        let size_score =
            (1.0 - (stats.avg_file_lines - 200.0).abs().max(0.0) / 800.0).max(0.0) * 100.0;

        // === Dimension 2: Dependency Health ===
        let cycle_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM (
                SELECT DISTINCT e1.source_file_id
                FROM code_graph_edges e1
                JOIN code_graph_edges e2 ON e1.target_file_id = e2.source_file_id
                    AND e2.target_file_id = e1.source_file_id
                WHERE e1.project_id = $1 AND e1.edge_type = 'import'
                    AND e2.edge_type = 'import'
            ) t",
        )
        .bind(project_id)
        .fetch_one(&self.db_pool)
        .await
        .unwrap_or(0);
        let dep_score = if stats.file_count > 0 {
            (1.0 - cycle_count as f64 / stats.file_count as f64).max(0.0) * 100.0
        } else {
            100.0
        };

        // === Dimension 3: Test Quality ===
        let test_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM indexed_files
             WHERE project_id = $1 AND relative_path ~* '(test|spec|_test\\.|_spec\\.)'",
        )
        .bind(project_id)
        .fetch_one(&self.db_pool)
        .await
        .unwrap_or(0);
        let test_ratio = if stats.file_count > 0 {
            test_count as f64 / stats.file_count as f64
        } else {
            0.0
        };
        let test_score = (test_ratio * 5.0).min(1.0) * 100.0; // 20% test files = 100

        // === Dimension 4: Documentation ===
        let doc_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM indexed_files WHERE project_id = $1 AND language = 'markdown'",
        )
        .bind(project_id)
        .fetch_one(&self.db_pool)
        .await
        .unwrap_or(0);
        let doc_score = (doc_count as f64 / stats.file_count.max(1) as f64 * 10.0).min(1.0) * 100.0;

        // === Dimension 5: Code Churn ===
        let avg_churn: Option<f64> = sqlx::query_scalar(
            "SELECT AVG(churn_rate)::DOUBLE PRECISION FROM file_metrics WHERE project_id = $1",
        )
        .bind(project_id)
        .fetch_optional(&self.db_pool)
        .await
        .unwrap_or(None)
        .flatten();
        let churn_score = (1.0 - avg_churn.unwrap_or(0.0).min(5.0) / 5.0) * 100.0;

        // === Dimension 6: Bug Fix Ratio ===
        let avg_fix: Option<f64> = sqlx::query_scalar(
            "SELECT AVG(fix_commit_ratio)::DOUBLE PRECISION FROM file_metrics WHERE project_id = $1"
        )
        .bind(project_id)
        .fetch_optional(&self.db_pool)
        .await
        .unwrap_or(None)
        .flatten();
        let fix_score = (1.0 - avg_fix.unwrap_or(0.0) * 3.0).max(0.0) * 100.0;

        // === Dimension 7: Coupling ===
        let avg_coupling: Option<f64> = sqlx::query_scalar(
            "SELECT AVG(COALESCE(afferent_coupling,0) + COALESCE(efferent_coupling,0))::DOUBLE PRECISION
             FROM file_metrics WHERE project_id = $1"
        )
        .bind(project_id)
        .fetch_optional(&self.db_pool)
        .await
        .unwrap_or(None)
        .flatten();
        let coupling_score = (1.0 - avg_coupling.unwrap_or(0.0).min(20.0) / 20.0) * 100.0;

        // === Dimension 8: Complexity ===
        let high_complexity_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM indexed_files f
             JOIN projects p ON f.project_id = p.id
             WHERE p.name = $1 AND f.line_count > 500",
        )
        .bind(&params.project)
        .fetch_one(&self.db_pool)
        .await
        .unwrap_or(0);
        let complexity_score = if stats.file_count > 0 {
            (1.0 - high_complexity_count as f64 / stats.file_count as f64).max(0.0) * 100.0
        } else {
            100.0
        };

        // === Dimension 9: Team Distribution ===
        let avg_authors: Option<f64> = sqlx::query_scalar(
            "SELECT AVG(author_count)::DOUBLE PRECISION FROM file_metrics WHERE project_id = $1",
        )
        .bind(project_id)
        .fetch_optional(&self.db_pool)
        .await
        .unwrap_or(None)
        .flatten();
        // Bus factor: higher avg authors = better. 2+ is good.
        let team_score = (avg_authors.unwrap_or(1.0).min(4.0) / 4.0 * 100.0).min(100.0);

        // === Dimension 10: Freshness ===
        let avg_stale: Option<f64> = sqlx::query_scalar(
            "SELECT AVG(days_since_last_change)::DOUBLE PRECISION FROM file_metrics
             WHERE project_id = $1 AND days_since_last_change IS NOT NULL",
        )
        .bind(project_id)
        .fetch_optional(&self.db_pool)
        .await
        .unwrap_or(None)
        .flatten();
        let freshness_score = (1.0 - avg_stale.unwrap_or(0.0).min(365.0) / 365.0) * 100.0;

        let dimensions = vec![
            (
                "code_structure",
                size_score,
                "File size distribution and organization",
            ),
            (
                "dependency_health",
                dep_score,
                "Absence of circular dependencies",
            ),
            ("test_quality", test_score, "Test file coverage ratio"),
            ("documentation", doc_score, "Documentation file presence"),
            ("code_stability", churn_score, "Low change churn rate"),
            ("bug_fix_ratio", fix_score, "Low proportion of fix commits"),
            ("coupling", coupling_score, "Low inter-module coupling"),
            (
                "complexity",
                complexity_score,
                "Absence of overly complex files",
            ),
            ("team_distribution", team_score, "Multi-author bus factor"),
            ("freshness", freshness_score, "Recent activity on files"),
        ];

        let gpa: f64 = dimensions
            .iter()
            .map(|(_, s, _)| grade_gpa(letter_grade(*s)))
            .sum::<f64>()
            / dimensions.len() as f64;

        let dim_json: Vec<serde_json::Value> = dimensions
            .iter()
            .map(|(name, score, desc)| {
                let grade = letter_grade(*score);
                serde_json::json!({
                    "dimension": name,
                    "score": format!("{:.1}", score),
                    "grade": grade,
                    "description": desc,
                })
            })
            .collect();

        // ORR checklist
        let orr = serde_json::json!({
            "no_circular_deps": cycle_count == 0,
            "test_coverage": test_ratio >= 0.1,
            "has_documentation": doc_count > 0,
            "low_churn": avg_churn.unwrap_or(0.0) < 3.0,
            "low_fix_ratio": avg_fix.unwrap_or(0.0) < 0.3,
            "no_god_files": high_complexity_count < 5,
            "bus_factor_ok": avg_authors.unwrap_or(1.0) >= 1.5,
            "recently_maintained": avg_stale.unwrap_or(0.0) < 180.0,
        });

        let orr_pass = orr
            .as_object()
            .map(|o| o.values().all(|v| v.as_bool().unwrap_or(false)))
            .unwrap_or(false);

        // Filter for failures_only
        let filtered_dims = if format == "failures_only" {
            dim_json
                .iter()
                .filter(|d| {
                    let grade = d["grade"].as_str().unwrap_or("A");
                    grade == "C" || grade == "D" || grade == "F"
                })
                .cloned()
                .collect::<Vec<_>>()
        } else {
            dim_json
        };

        let result = serde_json::json!({
            "project": params.project,
            "gpa": format!("{:.2}", gpa),
            "overall_grade": letter_grade(gpa * 25.0),
            "dimensions": filtered_dims,
            "orr_checklist": orr,
            "orr_pass": orr_pass,
            "project_stats": {
                "files": stats.file_count,
                "lines": stats.total_lines,
                "avg_file_lines": format!("{:.0}", stats.avg_file_lines),
                "test_files": test_count,
                "doc_files": doc_count,
            },
            "guidance": if orr_pass {
                "Project passes Operational Readiness Review. Focus on improving dimensions with grade C or below."
            } else {
                "Project does NOT pass ORR. Address failing checklist items before deployment."
            },
        });

        let json = serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(format!("Serialization failed: {}", e), None))?;

        debug!(
            tool = "engineering_scorecard",
            gpa = %format!("{:.2}", gpa),
            orr_pass,
            duration_ms = start.elapsed().as_millis() as u64,
            "MCP tool completed",
        );

        Ok(CallToolResult::success(vec![Content::text(json)]))
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
fn agglomerative_cluster(
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
fn format_clustering_summary(
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
    #[allow(dead_code)] // Used by the bin crate (src/main.rs); lib has no internal caller.
    pub(crate) async fn call_tool_cli(
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
        self.log_broadcaster.add_peer(context.peer.clone());
    }

    // ── Completions ──────────────────────────────────────────────────────

    async fn complete(
        &self,
        request: CompleteRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CompleteResult, McpError> {
        super::completions::handle_complete(&self.db_pool, request).await
    }

    // ── Logging ──────────────────────────────────────────────────────────

    async fn set_level(
        &self,
        request: SetLevelRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        tracing::info!(level = ?request.level, "Client set logging level");
        self.log_broadcaster.set_level(request.level);
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
                let snapshot = self.stats.snapshot();
                let json = serde_json::to_string_pretty(&snapshot)
                    .map_err(|e| McpError::internal_error(format!("{}", e), None))?;
                return Ok(ReadResourceResult::new(vec![ResourceContents::text(
                    json,
                    request.uri.clone(),
                )]));
            }
            "pgmcp://projects" => {
                let projects = crate::db::queries::list_projects(&self.db_pool)
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
                let paths = crate::db::queries::project_tree(&self.db_pool, name, 10)
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
            let projects = crate::db::queries::list_projects(&self.db_pool)
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
            let file = crate::db::queries::read_file_by_relative_path(&self.db_pool, path)
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
                let (task_id, cancel_flag) = self.task_store.create_task("reindex");
                let task = self
                    .task_store
                    .get_task(&task_id)
                    .expect("Task was just created");

                let db_pool = self.db_pool.clone();
                let task_store = Arc::clone(&self.task_store);
                let log_broadcaster = Arc::clone(&self.log_broadcaster);

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
        Ok(ListTasksResult::new(self.task_store.list_tasks()))
    }

    async fn get_task_info(
        &self,
        request: GetTaskInfoParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetTaskResult, McpError> {
        match self.task_store.get_task(&request.task_id) {
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
        match self.task_store.get_result(&request.task_id) {
            Some(result) => Ok(GetTaskPayloadResult::new(result)),
            None => {
                // Check if task exists but has no result yet
                if self.task_store.get_task(&request.task_id).is_some() {
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
        match self.task_store.cancel_task(&request.task_id) {
            Some(task) => Ok(CancelTaskResult { meta: None, task }),
            None => Err(McpError::internal_error(
                format!("Task not found: {}", request.task_id),
                None,
            )),
        }
    }
}
