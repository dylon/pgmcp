//! Recommendation, trajectory & topic-analysis tool parameter types.
//!
//! Extracted verbatim from `server.rs` (B.2 god-file split). All structs
//! re-exported by `params/mod.rs` and, transitively, by `server.rs` so
//! `crate::mcp::server::<Name>Params` resolves for every tool body file.
#![allow(unused_imports)]

use super::*;
use rmcp::schemars;
use serde::Deserialize;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ModuleGrowthParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Single-file path (optional). When omitted, project-scope.")]
    pub file: Option<String>,
    #[schemars(description = "Time bucket: \"week\", \"month\" (default), or \"quarter\".")]
    pub bucket: Option<String>,
    #[schemars(description = "How many buckets back to look at (default: 12)")]
    pub lookback_buckets: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AdoptionLagParams {
    #[schemars(description = "Reference file (the modern implementation)")]
    pub new_file: String,
    #[schemars(description = "Project filter (optional)")]
    pub project: Option<String>,
    #[schemars(description = "Worktree filter: \"main\" (default) or \"all\"")]
    pub worktree_filter: Option<String>,
    #[schemars(description = "Minimum similarity for legacy candidates (default: 0.70)")]
    pub min_similarity: Option<f64>,
    #[schemars(
        description = "Minimum age in days for a file to be considered legacy (default: 180)"
    )]
    pub legacy_min_age_days: Option<i32>,
    #[schemars(description = "Maximum legacy usages to return (default: 30)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TechDebtBurnDownParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Time horizon: \"week\", \"month\" (default), or \"quarter\".")]
    pub time_horizon: Option<String>,
    #[schemars(description = "Number of engineers available (default: 1)")]
    pub engineer_count: Option<i32>,
    #[schemars(description = "Maximum items to consider (default: 50)")]
    pub limit: Option<i32>,
}

/// Tier 4 — `pr_scope_recommender` (min/recommended/max PR scope from a starter file).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PrScopeRecommenderParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Starter file (relative path, required)")]
    pub file: String,
    #[schemars(
        description = "Minimum co-change Jaccard for the recommended scope (default: 0.4)."
    )]
    pub co_change_min: Option<f64>,
    #[schemars(description = "Reverse-BFS depth for the maximum scope (default: 2).")]
    pub impact_depth: Option<i32>,
    #[schemars(
        description = "If true (default), include topic-neighbor files (chunks sharing the seed's \
                       dominant topic) in the maximum scope."
    )]
    pub include_topic_neighbors: Option<bool>,
}

/// Tier 4 — `hot_path_audit` (central + churning + bug-prone intersection).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct HotPathAuditParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Percentile threshold (default: 0.9 = top 10%). A file qualifies only if \
                       it sits in the top P% of pagerank, churn, AND fix_commit_ratio."
    )]
    pub percentile_threshold: Option<f64>,
    #[schemars(description = "Maximum hot paths to return (default: 20)")]
    pub limit: Option<i32>,
}

/// Tier 4 — `bus_factor_map` (knowledge-concentration risk per file).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BusFactorMapParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Filter to files in the top (1 - min_pagerank_percentile) of pagerank \
                       (default: 0.5 — top half). Less central files are filtered out."
    )]
    pub min_pagerank_percentile: Option<f64>,
    #[schemars(description = "Maximum files to return (default: 30)")]
    pub limit: Option<i32>,
}

/// Tier 4 — `reviewer_recommender` (rank reviewers by recent file ownership).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReviewerRecommenderParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Changed files (relative paths, required)")]
    pub files: Vec<String>,
    #[schemars(description = "Authors to exclude (e.g. the PR author's email). Optional.")]
    pub exclude_authors: Option<Vec<String>>,
    #[schemars(
        description = "Recency window in days for blame data (default: 365). Older blame is \
                       ignored — long-stale ownership isn't reviewer authority."
    )]
    pub recency_window_days: Option<i32>,
}

/// Tier 3 — `recommend_layering` (infer layered architecture, list violation edges).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RecommendLayeringParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Number of layers to bucket the project into (default: 4)")]
    pub num_layers: Option<usize>,
    #[schemars(
        description = "Minimum severity to report: \"low\", \"medium\", \"high\", \"critical\" \
                       (default: \"medium\"). Severity = number of layers an edge crosses."
    )]
    pub severity_threshold: Option<String>,
    #[schemars(description = "Maximum violations to return (default: 50)")]
    pub limit: Option<i32>,
    #[schemars(
        description = "Optional layer-name override (top to bottom). The default heuristic \
                       infers names from instability — unreliable for non-web codebases. Pass \
                       N names matching `num_layers`."
    )]
    pub layer_names: Option<Vec<String>>,
}

/// Tier 3 — `shotgun_surgery_fix` (consolidation recommender for shotgun-surgery smells).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ShotgunSurgeryFixParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Minimum co-change partners for a hub to qualify (default: 6). Mirrors \
                       the threshold used by design_smell_detection."
    )]
    pub min_partners: Option<i32>,
    #[schemars(description = "Minimum Jaccard co-change similarity (default: 0.2).")]
    pub min_coupling: Option<f64>,
    #[schemars(description = "Maximum hubs to return (default: 15)")]
    pub limit: Option<i32>,
}

/// Tier 3 — `fix_circular_dependency` (cycle-breaking edge selection).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FixCircularDependencyParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Maximum cycle length to enumerate per SCC (default: 10). Longer cycles \
                       are reported as the SCC summary only."
    )]
    pub max_cycle_length: Option<i32>,
    #[schemars(description = "Maximum fix candidates to return (default: 20)")]
    pub limit: Option<i32>,
    #[schemars(
        description = "Strategy preference: \"interface\", \"inversion\", or \"auto\" (default). \
                       Auto picks based on Ce/Ca/instability of the cycle nodes."
    )]
    pub prefer_strategy: Option<String>,
}

/// Tier 3 — `recommend_module_split` (split god files using chunk → topic mapping).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RecommendModuleSplitParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Minimum file line_count to consider for splitting (default: 500). \
                       Mirrors the god_class threshold used by design_smell_detection."
    )]
    pub min_lines: Option<i32>,
    #[schemars(description = "Maximum split candidates to return (default: 10)")]
    pub limit: Option<i32>,
    #[schemars(
        description = "Minimum number of distinct topic-groups required to recommend a split \
                       (default: 2). Files whose chunks all belong to one dominant topic get an \
                       `add_test` recommendation instead — they're cohesive."
    )]
    pub min_communities: Option<usize>,
    #[schemars(
        description = "If true, include per-chunk membership detail in the output. Default false."
    )]
    pub include_chunks: Option<bool>,
}

/// Tier 3 — `stale_zombie_detector` (graph + history-based dead-code identification).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StaleZombieParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Minimum days since last commit (default: 540 — ~18 months)")]
    pub min_days_idle: Option<i32>,
    #[schemars(
        description = "Maximum PageRank percentile (default: 0.25 — bottom 25%). Files above this \
                       are too central to be zombies."
    )]
    pub max_pagerank_pct: Option<f64>,
    #[schemars(description = "Maximum candidates to return (default: 30)")]
    pub limit: Option<i32>,
}

/// Tier 2 — `boilerplate_clusters` (codegen-worthy near-identical chunks).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BoilerplateClustersParams {
    #[schemars(
        description = "Minimum chunk-pair similarity (default: 0.96). Aggressive — boilerplate \
                       must be near-identical."
    )]
    pub min_similarity: Option<f64>,
    #[schemars(description = "Minimum chunks per cluster (default: 3)")]
    pub min_cluster_size: Option<usize>,
    #[schemars(
        description = "Minimum normalized Jaccard match ratio after identifier substitution \
                       (default: 0.99). Below this, the cluster is real-similarity rather than \
                       boilerplate."
    )]
    pub min_normalized_match: Option<f64>,
    #[schemars(description = "Filter by programming language")]
    pub language: Option<String>,
    #[schemars(description = "Filter pairs touching this project")]
    pub project: Option<String>,
    #[schemars(description = "Maximum clusters to return (default: 20)")]
    pub limit: Option<i32>,
    #[schemars(description = "Worktree filter: \"main\" (default) or \"all\".")]
    pub worktree_filter: Option<String>,
    #[schemars(
        description = "If true, include pairs whose two projects are worktrees of the same \
                       upstream repo. Default false."
    )]
    pub include_same_repo: Option<bool>,
}

/// Tier 2 — `pattern_abstraction_candidates` (trait/interface extraction at medium similarity).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PatternAbstractionParams {
    #[schemars(description = "Minimum chunk-pair similarity (default: 0.70)")]
    pub min_similarity: Option<f64>,
    #[schemars(
        description = "Maximum chunk-pair similarity, exclusive (default: 0.85). Above this is \
                       duplicate code, not pattern."
    )]
    pub max_similarity: Option<f64>,
    #[schemars(
        description = "Minimum FCM topic-membership score on both endpoints (default: 0.55). \
                       Above this means the chunks are confidently in the same topic."
    )]
    pub min_topic_membership: Option<f64>,
    #[schemars(description = "Minimum implementations per pattern candidate (default: 4)")]
    pub min_cluster_size: Option<usize>,
    #[schemars(description = "Filter by programming language")]
    pub language: Option<String>,
    #[schemars(description = "Restrict to pairs touching this project")]
    pub project: Option<String>,
    #[schemars(description = "Maximum candidates to return (default: 20)")]
    pub limit: Option<i32>,
    #[schemars(description = "Worktree filter: \"main\" (default) or \"all\".")]
    pub worktree_filter: Option<String>,
    #[schemars(
        description = "If true, include candidates whose two projects are worktrees of the same \
                       upstream repo. Default false."
    )]
    pub include_same_repo: Option<bool>,
}

/// Tier 2 — `extraction_candidates` (ranked extract-to-shared-crate; superset of refactoring_report).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExtractionCandidatesParams {
    #[schemars(description = "Minimum file-pair similarity (default: 0.85)")]
    pub min_similarity: Option<f64>,
    #[schemars(description = "Minimum projects spanned by a candidate (default: 2)")]
    pub min_projects: Option<usize>,
    #[schemars(description = "Filter by programming language")]
    pub language: Option<String>,
    #[schemars(description = "Maximum candidates to return (default: 20)")]
    pub limit: Option<i32>,
    #[schemars(
        description = "Worktree filter: \"main\" (default) restricts to canonical main projects; \
                       \"all\" includes feature-branch worktrees."
    )]
    pub worktree_filter: Option<String>,
    #[schemars(
        description = "If true, include refactor candidates whose two projects are worktrees / \
                       sibling clones of the same upstream repo. Default false."
    )]
    pub include_same_repo: Option<bool>,
    #[schemars(
        description = "If true (default), count the call sites that would have to update with the \
                       extraction. Set false to skip the extra graph query."
    )]
    pub include_call_sites: Option<bool>,
    #[schemars(
        description = "Risk tier filter: \"any\" (default), \"low\", \"low-med\". Drops candidates \
                       whose risk_tier exceeds the threshold."
    )]
    pub risk_threshold: Option<String>,
}

/// Tier 2 — `internal_dry` (DRY within one file, real-time).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InternalDryParams {
    #[schemars(
        description = "File reference: either `project:relative_path` (e.g. \"f1r3node:src/cli/mod.rs\") or absolute path"
    )]
    pub file: String,
    #[schemars(
        description = "Minimum intra-file chunk-pair similarity (default: 0.80). Lower than \
                       cross-project DRY because semantically related code in the same file \
                       has more shared context."
    )]
    pub min_similarity: Option<f64>,
    #[schemars(
        description = "Minimum chunks per proposed helper (default: 2). Single chunks are \
                       skipped — a helper extracted from one chunk isn't a DRY win."
    )]
    pub min_pairs_per_helper: Option<usize>,
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
