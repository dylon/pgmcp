//! Graph, architecture, prediction & SOTA graph/info-theory parameter types.
//!
//! Extracted verbatim from `server.rs` (B.2 god-file split). All structs
//! re-exported by `params/mod.rs` and, transitively, by `server.rs` so
//! `crate::mcp::server::<Name>Params` resolves for every tool body file.
#![allow(unused_imports)]

use super::*;
use rmcp::schemars;
use serde::Deserialize;

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
    /// Include cross-project import edges (a `use` into a crate in another
    /// indexed project). Default false = intra-project only. Cross-project
    /// targets are labeled `<project>:<path>`.
    #[schemars(
        description = "Include cross-project import edges (use into another indexed project's crate); default false. Cross-project targets are labeled <project>:<path>"
    )]
    pub include_cross_project: Option<bool>,
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
    /// Module bucketing: "depth" (directory levels, default) or "crate" (Cargo
    /// crate boundaries — the true package unit of a Rust workspace).
    #[schemars(
        description = "Module bucketing: \"depth\" (directory levels, default) or \"crate\" (Cargo crate boundaries for Rust workspaces)"
    )]
    pub bucketing: Option<String>,
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
    /// Whether to embed a typed `recommended_fix` action on each violation. Default true.
    /// Set false to reproduce the pre-2026-04-30 diagnostic-only output shape.
    #[schemars(
        description = "Whether to embed a typed recommended_fix action on each violation (default: true). \
                       Set false to reproduce the pre-2026-04-30 diagnostic-only shape."
    )]
    pub include_fixes: Option<bool>,
    /// Module path prefixes (relative to project root) to exempt from the
    /// god-module rule. Intentional one-file-per-tool / one-file-per-pattern
    /// catalogs would otherwise be mis-flagged. When omitted, pgmcp's
    /// canonical defaults apply (see `tool_architecture_violations` body).
    #[schemars(
        description = "Module path prefixes to exempt from the god-module rule (e.g. [\"src/patterns\", \"src/mcp/tools\", \"pgmcp-testing/tests\"]). When omitted, pgmcp's canonical defaults apply."
    )]
    pub excluded_god_module_prefixes: Option<Vec<String>>,
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
    /// Whether to embed a typed `recommended_fix` action on each smell. Default true.
    #[schemars(
        description = "Whether to embed a typed recommended_fix action on each smell (default: true). \
                       Set false to reproduce the pre-2026-04-30 diagnostic-only shape."
    )]
    pub include_fixes: Option<bool>,
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

// SOTA Phase 2 — graph algorithms (Seidman, Cohen, Tong, Brandes, Burt, Milo, Holme)
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KcoreAnalysisParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Minimum coreness to include (default: 0)")]
    pub min_core: Option<u32>,
    #[schemars(description = "Max files to return (default: 50)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KtrussAnalysisParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Minimum trussness to include (default: 3)")]
    pub min_truss: Option<u32>,
    #[schemars(description = "Max edges to return (default: 50)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PersonalizedPagerankParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Seed file paths (required, ≥1)")]
    pub seed_files: Vec<String>,
    #[schemars(description = "Damping factor (default: 0.85)")]
    pub damping: Option<f64>,
    #[schemars(description = "Max files to return (default: 50)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EdgeBetweennessParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max edges to return (default: 50)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StructuralHolesParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Sort: \"constraint_asc\" (default, brokers first) or \"constraint_desc\""
    )]
    pub sort: Option<String>,
    #[schemars(description = "Max files to return (default: 30)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MotifCensusParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AttackVulnerabilityParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Removal order: \"pagerank\" (default), \"betweenness\", or \"degree\""
    )]
    pub removal_order: Option<String>,
    #[schemars(description = "Max removal steps (default: 50)")]
    pub max_steps: Option<u32>,
}

// SOTA Phase 3 — information theory
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CompressionDistanceParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "First file path (required)")]
    pub file_a: String,
    #[schemars(description = "Second file path (required)")]
    pub file_b: String,
    #[schemars(description = "zstd compression level (default: 3)")]
    pub level: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CochangeMutualInformationParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Minimum joint commits required to include a pair (default: 3)")]
    pub min_support: Option<u32>,
    #[schemars(description = "Max pairs to return (default: 50)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ImportEntropyParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Sort: \"entropy_desc\" (default) or \"entropy_asc\"")]
    pub sort: Option<String>,
    #[schemars(description = "Max files to return (default: 30)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct IdentifierEntropyParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Sort: \"entropy_desc\" (default) or \"entropy_asc\"")]
    pub sort: Option<String>,
    #[schemars(description = "Max files to return (default: 30)")]
    pub limit: Option<i32>,
}

// SOTA Phase 4 — evolution + quality
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BusFactorParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Fraction of lines that must be unmaintained to count (default: 0.5)"
    )]
    pub threshold: Option<f64>,
    #[schemars(description = "Max files to return (default: 30)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KnowledgeSilosParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Minimum Herfindahl index to include (default: 0.7 = high concentration)"
    )]
    pub min_herfindahl: Option<f64>,
    #[schemars(description = "Max files to return (default: 30)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct OwnershipCouplingMismatchParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Minimum Jaccard coupling to include (default: 0.3)")]
    pub min_coupling: Option<f64>,
    #[schemars(description = "Minimum joint commits (default: 3)")]
    pub min_commits: Option<u32>,
    #[schemars(description = "Max pairs to return (default: 30)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DocCodeDriftParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Minimum cosine distance to include (default: 0.3)")]
    pub min_drift: Option<f64>,
    #[schemars(description = "Max directories to return (default: 30)")]
    pub limit: Option<i32>,
}
