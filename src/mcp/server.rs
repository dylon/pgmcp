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

/// Wrap a tool's delegated future in a `tokio::time::timeout`. Tools
/// that exceed their budget surface a structured `McpError` instead of
/// hanging the harness; clients see a recognizable error rather than
/// dropping the connection. Stage 4b of the pgmcp-utilization plan
/// (`~/.claude/plans/thoroughly-examine-home-dylon-workspace-melodic-cake.md`).
///
/// Default budget is 30 s. `reindex` is the only tool exempt — it can
/// run for minutes when re-indexing a large workspace, and its progress
/// is reported via the MCP task store, not the immediate response.
pub(crate) async fn timeout_wrap<F>(
    name: &str,
    secs: u64,
    fut: F,
) -> Result<CallToolResult, McpError>
where
    F: std::future::Future<Output = Result<CallToolResult, McpError>>,
{
    match tokio::time::timeout(std::time::Duration::from_secs(secs), fut).await {
        Ok(r) => r,
        Err(_) => Err(McpError::internal_error(
            format!("{} timed out after {}s", name, secs),
            None,
        )),
    }
}

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
    #[schemars(
        description = "If true, collapse cross-worktree duplicates (same file appearing \
                       in multiple worktrees / sibling clones of the same upstream repo) \
                       to a single canonical hit per (repo, relative_path). Default false: \
                       all hits are returned, including the same code on different branches."
    )]
    pub dedupe_worktrees: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TextSearchParams {
    #[schemars(description = "Full-text search query")]
    pub query: String,
    #[schemars(description = "Maximum number of results (default: 10)")]
    pub limit: Option<i32>,
    #[schemars(description = "Filter by programming language")]
    pub language: Option<String>,
    #[schemars(
        description = "If true, collapse cross-worktree duplicates (see semantic_search). \
                       Default false."
    )]
    pub dedupe_worktrees: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GrepParams {
    #[schemars(description = "Regex pattern to search for")]
    pub pattern: String,
    #[schemars(description = "Glob pattern to filter files (e.g. '*.rs')")]
    pub glob: Option<String>,
    #[schemars(description = "Maximum number of results (default: 10)")]
    pub limit: Option<i32>,
    #[schemars(
        description = "If true, collapse cross-worktree duplicates (see semantic_search). \
                       Default false."
    )]
    pub dedupe_worktrees: Option<bool>,
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
    #[schemars(
        description = "If true, also return matches in worktrees / sibling clones \
                       of the seed file's repo (same git_common_dir or \
                       git_root_commits). Default false — same-repo matches are \
                       excluded so cross-repo refactor candidates aren't drowned \
                       out by the same code on different branches."
    )]
    pub include_same_repo: Option<bool>,
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
    #[schemars(description = "If true, include duplicates whose two projects are \
                       worktrees / sibling clones of the same upstream repo \
                       (same git_common_dir or git_root_commits). Default false. \
                       Most operators want false: same-code-different-branch is \
                       not a refactor candidate.")]
    pub include_same_repo: Option<bool>,
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
    #[schemars(
        description = "If true, include refactor candidates whose two projects \
                       are worktrees / sibling clones of the same upstream repo. \
                       Default false."
    )]
    pub include_same_repo: Option<bool>,
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
    /// Collapse cross-worktree duplicates
    #[schemars(
        description = "If true, collapse cross-worktree duplicates (see semantic_search). \
                       Default false."
    )]
    pub dedupe_worktrees: Option<bool>,
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
pub struct OrientParams {
    #[schemars(description = "Project name (as shown by list_projects)")]
    pub project: String,
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

    #[tool(description = "Vector-similarity search across all indexed files. \
USE WHEN: query is conceptual ('error handling patterns', 'auth flow', 'how does X work'), \
cross-project, or you don't know the exact tokens to search for. \
DO NOT USE WHEN: you have an exact symbol/string and just need its locations — `grep` or \
the built-in `Grep` is faster. \
Filter by project name to scope results. Use project: \"claude\" to search past Claude \
Code session transcripts, memory files, and plans from ~/.claude/.")]
    async fn semantic_search(
        &self,
        Parameters(params): Parameters<SemanticSearchParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("semantic_search");
        timeout_wrap(
            "semantic_search",
            30,
            super::tools::tool_semantic_search::tool_semantic_search(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "PostgreSQL full-text search across all indexed files. \
USE WHEN: searching for exact keywords or phrases across multiple projects, with \
ranking by relevance. \
DO NOT USE WHEN: you only need to search the current cwd (built-in `Grep` is faster), \
or when the query is conceptual rather than lexical (use `semantic_search` instead). \
Filter by project; use project: \"claude\" to search Claude Code session transcripts.")]
    async fn text_search(
        &self,
        Parameters(params): Parameters<TextSearchParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("text_search");
        timeout_wrap(
            "text_search",
            30,
            super::tools::tool_text_search::tool_text_search(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Regex pattern search across all indexed files (PostgreSQL ~ operator). \
USE WHEN: searching for a regex across the full indexed codebase or across multiple \
projects, especially when the model has no idea which project the match is in. \
DO NOT USE WHEN: you only need to search within the current cwd or a specific small \
directory tree — the built-in `Grep` tool is faster and respects .gitignore. \
Returns file paths, line numbers, and matching snippets across all indexed projects."
    )]
    async fn grep(
        &self,
        Parameters(params): Parameters<GrepParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("grep");
        timeout_wrap(
            "grep",
            30,
            super::tools::tool_grep::tool_grep(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Read an indexed file by absolute path, returning its content along with \
indexing metadata. \
USE WHEN: reading a file that is part of an indexed project AND you want the metadata \
envelope (last_indexed_at, language, chunk count). \
DO NOT USE WHEN: reading a file you just wrote this turn (not yet indexed), reading a \
.gitignore'd file, or reading a file outside the indexed workspaces — use the built-in \
`Read` tool for those."
    )]
    async fn read_file(
        &self,
        Parameters(params): Parameters<ReadFileParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("read_file");
        timeout_wrap(
            "read_file",
            30,
            super::tools::tool_read_file::tool_read_file(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "List all discovered projects with file counts.")]
    async fn list_projects(&self) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("list_projects");
        timeout_wrap(
            "list_projects",
            30,
            super::tools::tool_list_projects::tool_list_projects(self.ctx()),
        )
        .await
    }

    #[tool(
        description = "Composite first-step orientation snapshot for a project. Bundles project metadata, language breakdown, depth-2 directory tree, key entry points (top files by PageRank), recently-changed files, and top topics into one call. USE WHEN: entering an unfamiliar codebase or starting a non-trivial task — call this before scattering across list_projects/project_tree/centrality_analysis. Returns a `health` envelope flagging stale graph metrics or missing topic data so you can interpret partial results correctly."
    )]
    async fn orient(
        &self,
        Parameters(params): Parameters<OrientParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("orient");
        timeout_wrap(
            "orient",
            30,
            super::tools::tool_orient::tool_orient(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "Project file tree limited by depth (depth=2 typical). \
USE WHEN: you want the structural overview of a project without enumerating every file \
yourself via `Glob`. \
DO NOT USE WHEN: you only need to glob within a specific subdirectory — the built-in \
`Glob` tool gives you exact pattern matching against the live filesystem. \
For unfamiliar projects, prefer `orient` which bundles project_tree, top topics, and key \
entry points.")]
    async fn project_tree(
        &self,
        Parameters(params): Parameters<ProjectTreeParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("project_tree");
        timeout_wrap(
            "project_tree",
            30,
            super::tools::tool_project_tree::tool_project_tree(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Indexed-file metadata envelope (size, language, line count, \
last_indexed_at, project name, chunk count). \
USE WHEN: you want a quick fingerprint of a file before deciding whether to read it, \
or before semantic_search/grep on it specifically. \
DO NOT USE WHEN: the file is not in the index (e.g., just written, .gitignore'd) — \
use the built-in `Bash: stat` or `Read` instead."
    )]
    async fn file_info(
        &self,
        Parameters(params): Parameters<FileInfoParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("file_info");
        timeout_wrap(
            "file_info",
            30,
            super::tools::tool_file_info::tool_file_info(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Get overall indexing statistics including file counts, search counts, and pool state."
    )]
    async fn index_stats(&self) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("index_stats");
        timeout_wrap(
            "index_stats",
            30,
            super::tools::tool_index_stats::tool_index_stats(self.ctx()),
        )
        .await
    }

    #[tool(
        description = "Trigger a full re-index of all workspaces. Clears the existing index and restarts indexing. Can be invoked as a long-running task."
    )]
    async fn reindex(&self) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("reindex");
        // No timeout: reindex can run for minutes on a large workspace.
        // Progress is reported via the MCP task store, not the immediate
        // response — wrapping in 30s would falsely fail every full reindex.
        super::tools::tool_reindex::tool_reindex(self.ctx()).await
    }

    #[tool(
        description = "Pairwise file comparison via chunk-level vector similarity. \
USE WHEN: confirming whether two files implement the same concept, deciding if a candidate \
refactor target is similar enough to merge, or auditing apparent duplicates. \
DO NOT USE WHEN: looking for unknown duplicates — use `find_similar_modules` or \
`find_duplicates` to discover them first. \
Always real-time (no batch dependency). Path syntax: project:relative or absolute. Returns \
overall similarity, chunk alignment, and a human-readable verdict."
    )]
    async fn compare_files(
        &self,
        Parameters(params): Parameters<CompareFilesParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("compare_files");
        timeout_wrap(
            "compare_files",
            30,
            super::tools::tool_compare_files::tool_compare_files(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Find files similar to a given one across all indexed projects. \
USE WHEN: looking for cross-project copies of a utility, identifying refactor candidates \
(modules that could share a library), or asking 'has someone else solved this?'. \
DO NOT USE WHEN: comparing two specific files — use `compare_files`. \
Queries the materialized similarity table (populated by periodic batch scan); aggregates \
chunk similarity to file-level avg/max/matching count."
    )]
    async fn find_similar_modules(
        &self,
        Parameters(params): Parameters<FindSimilarModulesParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("find_similar_modules");
        timeout_wrap(
            "find_similar_modules",
            30,
            super::tools::tool_find_similar_modules::tool_find_similar_modules(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Cross-project duplicate-code cluster discovery (union-find on similarity \
pairs). \
USE WHEN: looking for refactor opportunities across the user's whole indexed workspace, \
finding redundant utilities to consolidate, or auditing copy-paste violations. \
DO NOT USE WHEN: you already know what you're looking for — use `find_similar_modules` \
with a seed file. \
Filters to clusters spanning min_projects+ distinct projects. Requires the similarity \
batch scan to have run at least once."
    )]
    async fn find_duplicates(
        &self,
        Parameters(params): Parameters<FindDuplicatesParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("find_duplicates");
        timeout_wrap(
            "find_duplicates",
            30,
            super::tools::tool_find_duplicates::tool_find_duplicates(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Generate an actionable refactoring report identifying code that could be extracted into shared libraries. Builds on find_duplicates clustering with richer analysis: suggests crate names from common path segments, estimates shared lines, and ranks by project_count * avg_similarity. Requires the similarity batch scan to have run at least once."
    )]
    async fn refactoring_report(
        &self,
        Parameters(params): Parameters<RefactoringReportParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("refactoring_report");
        timeout_wrap(
            "refactoring_report",
            30,
            super::tools::tool_refactoring_report::tool_refactoring_report(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "Semantic search over git commit messages and diffs. \
USE WHEN: investigating when a feature was added, when a bug was fixed, how a piece of \
code evolved, or who last touched a concept ('fix database timeout', 'add authentication'). \
DO NOT USE WHEN: you have an exact commit hash (`git show <hash>` is faster) or you only \
need recent commits in the current cwd (`git log` is faster). \
Requires per-project opt-in via [git] index_history = true in .pgmcp.toml.")]
    async fn search_commits(
        &self,
        Parameters(params): Parameters<SearchCommitsParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("search_commits");
        timeout_wrap(
            "search_commits",
            30,
            super::tools::tool_search_commits::tool_search_commits(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Discover semantic code patterns via Fuzzy C-Means clustering on chunk \
embeddings (Fuzzy BERTopic + c-TF-IDF labels). \
USE WHEN: you want to understand the dominant patterns/concerns in a project (intra-project \
DRY violations) or shared patterns across projects (cross-project library candidates). \
DO NOT USE WHEN: you already know the concept and want to find specific instances — use \
`semantic_search` instead. \
With `project`: real-time intra-project. Without: cached cross-project results. Returns \
topic clusters with keyword labels, membership scores, and representative chunks/files."
    )]
    async fn discover_topics(
        &self,
        Parameters(params): Parameters<DiscoverTopicsParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("discover_topics");
        timeout_wrap(
            "discover_topics",
            30,
            super::tools::tool_discover_topics::tool_discover_topics(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Meta-clustering hierarchy over global topic centroids (Phase 9). Returns FCM-based meta-groups where each meta-group's parent_topic_ids point to the global topics it contains. Complementary view to discover_topics — chunk-to-global-topic assignments remain authoritative for cross-document comparability."
    )]
    async fn topic_hierarchy_fcm(
        &self,
        Parameters(params): Parameters<TopicHierarchyFcmParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("topic_hierarchy_fcm");
        timeout_wrap(
            "topic_hierarchy_fcm",
            30,
            super::tools::tool_topic_hierarchy_fcm::tool_topic_hierarchy_fcm(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Find chunks/files with low topic membership (below threshold). \
USE WHEN: looking for dead code, abandoned utilities, or candidates for deletion. Orphan \
code is content the topic model couldn't fit anywhere with confidence. \
DO NOT USE WHEN: looking for files whose semantic doesn't match their directory — use \
`find_misplaced_code` for that. \
Requires discover_topics first."
    )]
    async fn find_orphans(
        &self,
        Parameters(params): Parameters<FindOrphansParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("find_orphans");
        timeout_wrap(
            "find_orphans",
            30,
            super::tools::tool_find_orphans::tool_find_orphans(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Architecture-recovery: files whose semantic topic doesn't match their \
directory context. \
USE WHEN: looking for files in the wrong module, suggesting reorganization, or auditing \
'why is this in this folder?'. \
DO NOT USE WHEN: looking for orphans (no topic) — use `find_orphans`. \
Compares each file's dominant topic vs its directory neighbors' majority. Requires \
discover_topics first."
    )]
    async fn find_misplaced_code(
        &self,
        Parameters(params): Parameters<FindMisplacedCodeParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("find_misplaced_code");
        timeout_wrap(
            "find_misplaced_code",
            30,
            super::tools::tool_find_misplaced_code::tool_find_misplaced_code(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Find files that frequently change together in git commits (Jaccard \
co-change coupling). \
USE WHEN: planning a refactor and want to know which files will likely need to change \
together, or assessing whether two files belong in the same module. High coupling >0.7 \
suggests strong implicit dependency. \
DO NOT USE WHEN: looking for static dependencies (use `dependency_graph` instead) or \
semantic similarity (use `find_similar_modules`). \
Requires [git] index_history = true."
    )]
    async fn find_coupled_files(
        &self,
        Parameters(params): Parameters<FindCoupledFilesParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("find_coupled_files");
        timeout_wrap(
            "find_coupled_files",
            30,
            super::tools::tool_find_coupled_files::tool_find_coupled_files(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Find topics with implementation code but no test coverage. \
USE WHEN: building a test plan, identifying which subsystems have weak tests, or arguing \
for resourcing test work in specific areas. \
DO NOT USE WHEN: you want line-coverage data — pgmcp doesn't run the tests, only \
classifies files as test/impl based on path heuristics. Use a coverage tool (tarpaulin, \
llvm-cov) for true coverage. \
Requires discover_topics first."
    )]
    async fn test_coverage_gaps(
        &self,
        Parameters(params): Parameters<TestCoverageGapsParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("test_coverage_gaps");
        timeout_wrap(
            "test_coverage_gaps",
            30,
            super::tools::tool_test_coverage_gaps::tool_test_coverage_gaps(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Rank files by composite complexity (size + chunk count + topic diversity \
+ coupling). \
USE WHEN: identifying SRP violations, finding files that 'do too much', or prioritizing \
refactor targets by raw size/diversity. \
DO NOT USE WHEN: you want bug-likelihood (use `bug_prediction`) or formal complexity \
metrics (use `design_metrics` for cyclomatic + WMC + maintainability index). \
Sortable by: composite (default), size, chunks, topics, coupling."
    )]
    async fn complexity_hotspots(
        &self,
        Parameters(params): Parameters<ComplexityHotspotsParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("complexity_hotspots");
        timeout_wrap(
            "complexity_hotspots",
            30,
            super::tools::tool_complexity_hotspots::tool_complexity_hotspots(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Show how discovered topics relate hierarchically using agglomerative clustering on topic centroids. Reveals module boundaries and related topic groups. Groups with low merge distance contain highly related topics that could be combined."
    )]
    async fn topic_hierarchy(
        &self,
        Parameters(params): Parameters<TopicHierarchyParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("topic_hierarchy");
        timeout_wrap(
            "topic_hierarchy",
            30,
            super::tools::tool_topic_hierarchy::tool_topic_hierarchy(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Find files (default: markdown) covering overlapping topics that should \
be consolidated. \
USE WHEN: cleaning up a docs/ directory with redundant pages, or finding code modules \
that duplicate concerns. \
DO NOT USE WHEN: looking for line-level duplicates — use `find_duplicates`. This is \
topic-level, not text-level. \
Weighted Jaccard on per-file topic distributions, union-find clustered. Set language=\"*\" \
for all languages."
    )]
    async fn suggest_merges(
        &self,
        Parameters(params): Parameters<SuggestMergesParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("suggest_merges");
        timeout_wrap(
            "suggest_merges",
            30,
            super::tools::tool_suggest_merges::tool_suggest_merges(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Find files spanning too many distinct topics and suggest split points. \
USE WHEN: a markdown file or source module has grown sprawling, or you suspect an SRP \
violation that you want broken up cleanly. \
DO NOT USE WHEN: looking for general complexity hotspots — use `complexity_hotspots`. \
Splits align to heading boundaries (markdown) or chunk boundaries (code). Shannon-entropy \
scored. Requires discover_topics first."
    )]
    async fn suggest_splits(
        &self,
        Parameters(params): Parameters<SuggestSplitsParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("suggest_splits");
        timeout_wrap(
            "suggest_splits",
            30,
            super::tools::tool_suggest_splits::tool_suggest_splits(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Find code topics with no corresponding markdown documentation. \
USE WHEN: building a docs-debt list, finding sub-systems that exist only in code, or \
prioritizing where to write documentation. \
DO NOT USE WHEN: you want to assess docstring quality (comments inside code) — this only \
considers separate markdown files. \
Requires discover_topics first."
    )]
    async fn doc_coverage_gaps(
        &self,
        Parameters(params): Parameters<DocCoverageGapsParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("doc_coverage_gaps");
        timeout_wrap(
            "doc_coverage_gaps",
            30,
            super::tools::tool_doc_coverage_gaps::tool_doc_coverage_gaps(self.ctx(), params),
        )
        .await
    }

    // ========================================================================
    // Phase 2: Graph Analysis tools
    // ========================================================================

    #[tool(
        description = "Project dependency graph: import relationships, optionally focused on a \
file's neighborhood. \
USE WHEN: you need to know what depends on a file, what a file depends on, or want a \
Graphviz diagram of an architecture. \
DO NOT USE WHEN: you need co-change behavior (use `find_coupled_files`) or static call \
graphs (this is import-level only). \
Output formats: summary (counts), edges (list), DOT (Graphviz). Requires graph-analysis cron."
    )]
    async fn dependency_graph(
        &self,
        Parameters(params): Parameters<DependencyGraphParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("dependency_graph");
        timeout_wrap(
            "dependency_graph",
            30,
            super::tools::tool_dependency_graph::tool_dependency_graph(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Rank files by graph centrality (PageRank, betweenness, degree). \
USE WHEN: identifying load-bearing files in an unfamiliar codebase ('what should I read \
first?'), or finding which files a refactor would impact most. High-centrality = touches \
many other files. \
DO NOT USE WHEN: you want change-frequency or bug-proneness — use `bug_prediction` or \
`complexity_hotspots`. \
Requires graph-analysis cron. The composite `orient` tool returns the top entry points by \
PageRank as part of its envelope."
    )]
    async fn centrality_analysis(
        &self,
        Parameters(params): Parameters<CentralityAnalysisParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("centrality_analysis");
        timeout_wrap(
            "centrality_analysis",
            30,
            super::tools::tool_centrality_analysis::tool_centrality_analysis(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Detect module communities in the dependency graph using Louvain algorithm. Compares discovered communities against directory structure to reveal architectural misalignment. Requires the graph-analysis cron job to have run."
    )]
    async fn community_detection(
        &self,
        Parameters(params): Parameters<CommunityDetectionParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("community_detection");
        timeout_wrap(
            "community_detection",
            30,
            super::tools::tool_community_detection::tool_community_detection(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Find circular import dependency cycles (Tarjan SCC + DFS). \
USE WHEN: investigating build/link errors, code that's hard to test in isolation, or \
auditing layering violations. Cycles make code harder to test, build, and understand. \
DO NOT USE WHEN: looking for runtime call cycles (this is import-level static graph only). \
Requires graph-analysis cron."
    )]
    async fn circular_dependencies(
        &self,
        Parameters(params): Parameters<CircularDependenciesParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("circular_dependencies");
        timeout_wrap(
            "circular_dependencies",
            30,
            super::tools::tool_circular_dependencies::tool_circular_dependencies(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Predict which files would be affected by changing a specific file. \
USE WHEN: scoping a refactor or assessing the blast radius of a change before making it. \
Combines reverse-imports + git co-change + semantic similarity for richer impact than any \
single signal. \
DO NOT USE WHEN: you only need static reverse-imports (use `dependency_graph` with focus). \
Requires graph-analysis cron + git history for full coverage."
    )]
    async fn change_impact_analysis(
        &self,
        Parameters(params): Parameters<ChangeImpactAnalysisParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("change_impact_analysis");
        timeout_wrap(
            "change_impact_analysis",
            30,
            super::tools::tool_change_impact_analysis::tool_change_impact_analysis(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    // ========================================================================
    // Phase 3: Architecture & Design Quality tools
    // ========================================================================

    #[tool(
        description = "Robert C. Martin package metrics per module: Ca, Ce, Instability (I), \
Abstractness (A), Distance from Main Sequence (D*). \
USE WHEN: doing a formal architecture review, identifying Zone of Pain (low A, low I) or \
Zone of Uselessness (high A, high I) modules. \
DO NOT USE WHEN: looking at single-file complexity — use `design_metrics`. This is \
module/package level. \
Requires graph-analysis cron."
    )]
    async fn coupling_cohesion_report(
        &self,
        Parameters(params): Parameters<CouplingCohesionReportParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("coupling_cohesion_report");
        timeout_wrap(
            "coupling_cohesion_report",
            30,
            super::tools::tool_coupling_cohesion_report::tool_coupling_cohesion_report(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Detect architecture violations: cycles, god modules, bidirectional deps, \
SDP violations, Zone of Pain/Uselessness modules. \
USE WHEN: producing an architecture review, gating a PR on architectural-debt regressions, \
or building an ORR (Operational Readiness Review). \
DO NOT USE WHEN: looking at design-level smells in a single file — use \
`design_smell_detection` for god class / SRP violations / shotgun surgery / etc. \
Grouped by severity. Requires graph-analysis cron."
    )]
    async fn architecture_violations(
        &self,
        Parameters(params): Parameters<ArchitectureViolationsParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("architecture_violations");
        timeout_wrap(
            "architecture_violations",
            30,
            super::tools::tool_architecture_violations::tool_architecture_violations(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "File-level design smells: god class, SRP violation, shotgun surgery, \
stale module, unstable dependency. \
USE WHEN: doing a code review for design quality, finding refactor targets at the file \
level. Each smell has a clear remediation pattern. \
DO NOT USE WHEN: looking for module/package-level violations — use `architecture_violations` \
for those. \
Filter to specific smell types via `smells` param. Requires graph-analysis + discover_topics."
    )]
    async fn design_smell_detection(
        &self,
        Parameters(params): Parameters<DesignSmellDetectionParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("design_smell_detection");
        timeout_wrap(
            "design_smell_detection",
            30,
            super::tools::tool_design_smell_detection::tool_design_smell_detection(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "10-dimension architecture-quality scorecard (separation of concerns, \
loose coupling, SDP compliance, acyclicity, test coverage, doc coverage, code organization, \
module balance, API stability, dependency health). \
USE WHEN: producing an architecture review or maturity assessment, comparing two projects \
on aggregate quality. \
DO NOT USE WHEN: you want the full A-F engineering scorecard with ORR checklist — use \
`engineering_scorecard` (this tool is one of its inputs). \
Each dim 0-100%. Requires graph-analysis + discover_topics."
    )]
    async fn architecture_quality(
        &self,
        Parameters(params): Parameters<ArchitectureQualityParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("architecture_quality");
        timeout_wrap(
            "architecture_quality",
            30,
            super::tools::tool_architecture_quality::tool_architecture_quality(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Per-file design metrics: cyclomatic complexity, WMC, Card & Glass S/D/Sy, \
maintainability index. \
USE WHEN: ranking refactor targets by formal numeric metrics, or comparing complexity \
between two files objectively. \
DO NOT USE WHEN: you want a composite ranking (use `complexity_hotspots`) or bug \
prediction (use `bug_prediction`). \
Pure metrics, no interpretation. Useful in scorecards and CI gates."
    )]
    async fn design_metrics(
        &self,
        Parameters(params): Parameters<DesignMetricsParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("design_metrics");
        timeout_wrap(
            "design_metrics",
            30,
            super::tools::tool_design_metrics::tool_design_metrics(self.ctx(), params),
        )
        .await
    }

    // ========================================================================
    // Phase 4: ML Prediction tools (heuristic-based, no ML dependencies)
    // ========================================================================

    #[tool(
        description = "Heuristic bug-proneness ranking per file (churn × complexity × fix-commit \
ratio × coupling). \
USE WHEN: prioritizing review/test-coverage effort, or identifying risky files to refactor \
first. \
DO NOT USE WHEN: looking at a single file (use `complexity_hotspots` and \
`technical_debt_analysis` for richer per-file detail). \
Heuristic, not ML. Requires graph-analysis cron + git history."
    )]
    async fn bug_prediction(
        &self,
        Parameters(params): Parameters<BugPredictionParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("bug_prediction");
        timeout_wrap(
            "bug_prediction",
            30,
            super::tools::tool_bug_prediction::tool_bug_prediction(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Composite technical-debt score per file (TODO density + cyclomatic \
complexity + test gaps + D* + churn). \
USE WHEN: building a refactor backlog, identifying highest-leverage cleanup targets, or \
estimating debt for an architecture review. \
DO NOT USE WHEN: looking at a specific file's complexity in isolation — `design_metrics` \
gives per-file numbers without the composite weighting. \
Optionally scans content for TODO/FIXME/HACK markers."
    )]
    async fn technical_debt_analysis(
        &self,
        Parameters(params): Parameters<TechnicalDebtAnalysisParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("technical_debt_analysis");
        timeout_wrap(
            "technical_debt_analysis",
            30,
            super::tools::tool_technical_debt_analysis::tool_technical_debt_analysis(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Statistical outlier detection: files whose embedding distance from \
project centroid + metric z-scores deviate from the project norm. \
USE WHEN: hunting for abandoned experiments, copy-pasted code from other projects, or \
architectural inconsistencies the model can't see by reading any single file. \
DO NOT USE WHEN: looking for misplaced files relative to directory context — use \
`find_misplaced_code` (semantic-based, more targeted). \
No ML deps — pure statistical distance."
    )]
    async fn anomaly_detection(
        &self,
        Parameters(params): Parameters<AnomalyDetectionParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("anomaly_detection");
        timeout_wrap(
            "anomaly_detection",
            30,
            super::tools::tool_anomaly_detection::tool_anomaly_detection(self.ctx(), params),
        )
        .await
    }

    // ========================================================================
    // Phase 5: NLP & IR tools
    // ========================================================================

    #[tool(
        description = "Combined keyword + semantic search using Reciprocal Rank Fusion (RRF). \
Runs BM25 full-text and vector similarity in parallel, merges with configurable weights. \
USE WHEN: query is partially lexical and partially conceptual ('async error handling'), \
or you want robust ranking when neither pure keyword nor pure semantic alone gets the \
right top result. \
DO NOT USE WHEN: query is purely lexical (text_search is sufficient) or purely \
conceptual (semantic_search is sufficient). \
RRF gives more stable ordering than either branch alone for mixed queries."
    )]
    async fn hybrid_search(
        &self,
        Parameters(params): Parameters<HybridSearchParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("hybrid_search");
        timeout_wrap(
            "hybrid_search",
            30,
            super::tools::tool_hybrid_search::tool_hybrid_search(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Structural summary of a project, directory, or specific file. \
USE WHEN: writing a module's README, explaining unfamiliar code to someone, or generating \
a design-doc starting point. Combines PageRank-ranked key modules + topic assignments + \
language breakdown into prose. \
DO NOT USE WHEN: you only need a directory listing — use `project_tree`. \
Requires graph-analysis cron and discover_topics. The `orient` tool gives a faster \
project-wide overview without prose."
    )]
    async fn code_summarize(
        &self,
        Parameters(params): Parameters<CodeSummarizeParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("code_summarize");
        timeout_wrap(
            "code_summarize",
            30,
            super::tools::tool_code_summarize::tool_code_summarize(self.ctx(), params),
        )
        .await
    }

    // ========================================================================
    // Phase 6: Engineering Scorecard
    // ========================================================================

    #[tool(
        description = "Engineering-quality scorecard: 10 dimensions A-F + GPA + ORR checklist. \
USE WHEN: producing a quarterly health report for a service, evaluating whether a project \
is ready for production handoff, or comparing the maturity of two projects. \
DO NOT USE WHEN: you only need a single dimension — call the underlying tool directly \
(`architecture_quality`, `bug_prediction`, `test_coverage_gaps`, etc.). \
Aggregates dependency analysis + architecture quality + design smells + test/doc coverage \
+ health metrics. Requires graph-analysis cron + discover_topics."
    )]
    async fn engineering_scorecard(
        &self,
        Parameters(params): Parameters<EngineeringScorecardParams>,
    ) -> Result<CallToolResult, McpError> {
        self.stats().record_tool_call("engineering_scorecard");
        timeout_wrap(
            "engineering_scorecard",
            30,
            super::tools::tool_engineering_scorecard::tool_engineering_scorecard(
                self.ctx(),
                params,
            ),
        )
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
        .with_server_info(Implementation::new("pgmcp", env!("CARGO_PKG_VERSION")))
        .with_instructions(
            "pgmcp indexes the user's development workspaces into PostgreSQL+pgvector and \
             exposes ~40 tools for cross-project search, semantic queries, graph analysis, \
             and code-health metrics.\n\n\
             USE THESE TOOLS BEFORE built-in Read/Grep/Glob when the question is conceptual \
             ('how does X work?'), cross-project ('does this pattern exist elsewhere?'), \
             graph-shaped ('what depends on this?'), or about code health ('where is the \
             technical debt?'). Built-in tools remain right for narrow within-cwd operations \
             and for files just written this turn (not yet in the index).\n\n\
             FIRST STEP for unfamiliar codebases or non-trivial tasks: call `orient` — it \
             bundles project_tree, key entry points by PageRank, recently-changed files, \
             top topics, and a `health` envelope into one call so you don't have to scatter \
             across half a dozen tools to get oriented.\n\n\
             The 'claude' project indexes ~/.claude/ — past Claude Code sessions, memory \
             files, plans. Use semantic_search or text_search with project: \"claude\" to \
             retrieve prior context, decisions, and plans.\n\n\
             ### Tool catalog\n\n\
             SEARCH: orient (composite first-step), semantic_search (vector similarity, \
             conceptual queries), text_search (Postgres full-text, exact keywords), \
             grep (regex across all indexed files), hybrid_search (BM25+vector RRF — best \
             for queries that benefit from both keyword and concept), search_commits (git \
             history semantic search; requires [git] index_history = true).\n\n\
             READ/INVENTORY: read_file, file_info, list_projects, project_tree, index_stats.\n\n\
             CROSS-PROJECT SIMILARITY: compare_files (real-time chunk-level), \
             find_similar_modules (materialized table), find_duplicates (union-find \
             clusters), refactoring_report (actionable extraction candidates).\n\n\
             TOPIC DISCOVERY (Fuzzy BERTopic = FCM + c-TF-IDF): discover_topics, \
             topic_hierarchy, topic_hierarchy_fcm — soft-clustering chunks into \
             keyword-labeled topics. With project param = real-time intra-project; \
             without = cached cross-project.\n\n\
             CODE ANALYSIS: find_orphans (low topic membership), find_misplaced_code \
             (semantic vs directory mismatch), find_coupled_files (git co-change Jaccard), \
             test_coverage_gaps, complexity_hotspots, doc_coverage_gaps, suggest_merges, \
             suggest_splits.\n\n\
             GRAPH: dependency_graph (DOT/edges/summary), centrality_analysis (PageRank, \
             betweenness, degree), community_detection (Louvain), circular_dependencies \
             (Tarjan SCC), change_impact_analysis (graph + co-change + semantic).\n\n\
             ARCHITECTURE & DESIGN: coupling_cohesion_report (Robert C. Martin Ca/Ce/I/A/D*), \
             architecture_violations, design_smell_detection (god class, SRP violation, \
             shotgun surgery, stale module, unstable dependency), architecture_quality \
             (10-dim 0-100% scorecard), design_metrics (cyclomatic, WMC, maintainability).\n\n\
             PREDICTION: bug_prediction (churn × complexity × fix ratio), \
             technical_debt_analysis (TODO density + complexity + test gaps + churn), \
             anomaly_detection (embedding distance from project centroid).\n\n\
             SUMMARIZATION & SCORECARD: code_summarize (structural roll-up), \
             engineering_scorecard (10-dim A-F + GPA + ORR checklist).",
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
