//! SOTA concurrency, safety, security, API, ML & data-engineering parameter types.
//!
//! Extracted verbatim from `server.rs` (B.2 god-file split). All structs
//! re-exported by `params/mod.rs` and, transitively, by `server.rs` so
//! `crate::mcp::server::<Name>Params` resolves for every tool body file.
#![allow(unused_imports)]

use super::*;
use rmcp::schemars;
use serde::Deserialize;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TestSmellsParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max files to return (default: 50)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MutationScoreSurrogateParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max files to return (default: 50)")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FlakyTestCandidatesParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max test files to return (default: 30)")]
    pub limit: Option<i32>,
}

// SOTA Phase 5 — concurrency / safety / performance
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LocksetRacesParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max matches to return (default: 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UnsafeClustersParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max files to return (default: 25)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PanicPathsParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Entry filter: \"any\" (default), \"pub\", \"module\", \"private\"")]
    pub entry_filter: Option<String>,
    #[schemars(description = "Max functions to return (default: 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CentralFunctionsParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Ranking metric: \"pagerank\" (default), \"betweenness\", \"harmonic\", or \"coreness\""
    )]
    pub metric: Option<String>,
    #[schemars(description = "Max functions to return (default: 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FunctionCommunitiesParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Minimum community size to report (default: 2)")]
    pub min_size: Option<i32>,
    #[schemars(description = "Max communities to return, largest first (default: 30)")]
    pub limit: Option<i32>,
    #[schemars(description = "Max member functions listed per community (default: 15)")]
    pub members_per_community: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FunctionKcoreParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Minimum coreness to report (default: 2)")]
    pub min_coreness: Option<i32>,
    #[schemars(description = "Max functions to return (default: 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RecursiveClustersParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max simple-cycle length to enumerate per cluster (default: 8)")]
    pub max_cycle_len: Option<i32>,
    #[schemars(description = "Max recursion clusters to return, largest first (default: 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExtendedCentralityParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Metric: \"eigenvector\" (default), \"katz\", \"harmonic\", \"closeness\", or \"reverse_pagerank\""
    )]
    pub metric: Option<String>,
    #[schemars(
        description = "Graph scope: \"file\" (import graph, default) or \"function\" (call graph)"
    )]
    pub scope: Option<String>,
    #[schemars(description = "Max nodes to return (default: 50)")]
    pub limit: Option<i32>,
    #[schemars(
        description = "Katz attenuation factor alpha (default: 0.1); only used for metric=katz"
    )]
    pub alpha: Option<f64>,
    #[schemars(description = "Katz base constant beta (default: 1.0); only used for metric=katz")]
    pub beta: Option<f64>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ArticulationPointsParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Graph scope: \"file\" (import graph, default) or \"function\" (call graph)"
    )]
    pub scope: Option<String>,
    #[schemars(description = "Max cut vertices and bridges to return (default: 100)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GraphConnectivityParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Graph scope: \"file\" (import graph, default) or \"function\" (call graph)"
    )]
    pub scope: Option<String>,
    #[schemars(description = "Max components / partition members to list (default: 20)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CkMetricsParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Sort key: wmc (default) | dit | noc | cbo | rfc")]
    pub sort: Option<String>,
    #[schemars(description = "Max classes to return (default: 40)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SpectralAnalysisParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Graph scope: \"file\" (import graph, default) or \"function\" (call graph)"
    )]
    pub scope: Option<String>,
    #[schemars(description = "WL refinement rounds for structural clones (default: 2, max 6)")]
    pub wl_iterations: Option<i32>,
    #[schemars(description = "Max bisection members / clone classes to list (default: 20)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ArchitectureDsmParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Graph scope: \"file\" (import graph, default) or \"function\" (call graph)"
    )]
    pub scope: Option<String>,
    #[schemars(
        description = "Max files per ranked list (top-by-VFI, top-by-VFO, cyclic core); default 20"
    )]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CodePprSearchParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Natural-language or code query (required)")]
    pub query: String,
    #[schemars(description = "Number of result files to return (default: 10, max 100)")]
    pub k: Option<i32>,
    #[schemars(description = "Dense seed files to restart PageRank on (default: 10, max 100)")]
    pub max_seeds: Option<i32>,
    #[schemars(description = "PageRank damping/restart factor alpha in [0,1] (default: 0.85)")]
    pub alpha: Option<f64>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CodePathSearchParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Natural-language or code query (required)")]
    pub query: String,
    #[schemars(description = "Number of ranked paths to return (default: 15, max 200)")]
    pub k: Option<i32>,
    #[schemars(description = "Dense seed files to start paths from (default: 5, max 50)")]
    pub max_seeds: Option<i32>,
    #[schemars(description = "Maximum edges per path (default: 4, max 6)")]
    pub max_hops: Option<i32>,
    #[schemars(
        description = "Prune a path once its accumulated flow (product of edge weights) drops below \
this; in [0,1], default 0.1"
    )]
    pub min_flow: Option<f64>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CodeRaptorSearchParams {
    #[schemars(
        description = "Project name to scope to; omit to search conceptual summaries across ALL projects"
    )]
    pub project: Option<String>,
    #[schemars(description = "Conceptual query (required)")]
    pub query: String,
    #[schemars(description = "Number of summaries to return (default: 10, max 100)")]
    pub k: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct HitsParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Graph scope: \"file\" (import graph, default) or \"function\" (call graph)"
    )]
    pub scope: Option<String>,
    #[schemars(description = "Max hubs and authorities to return (default: 25)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DominatorTreeParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Graph scope: \"file\" (import graph, default) or \"function\" (call graph)"
    )]
    pub scope: Option<String>,
    #[schemars(
        description = "Root/entry node (exact label else substring); default = highest-out-degree node"
    )]
    pub root: Option<String>,
    #[schemars(description = "Max chokepoints to return (default: 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DeadlockCandidatesParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DeadlockCyclesParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Max call hops for interprocedural lock inlining (default 5, clamp 1..12)"
    )]
    pub max_call_depth: Option<u32>,
    #[schemars(
        description = "Drop lock-order edges below this resource-key confidence (default 0.3)"
    )]
    pub confidence_floor: Option<f32>,
    #[schemars(description = "Max simple-cycle length to enumerate (default 6, clamp 2..12)")]
    pub max_cycle_len: Option<u32>,
    #[schemars(description = "Include all-read (non-deadlocking) rwlock cycles (default false)")]
    pub include_low_confidence: Option<bool>,
    #[schemars(description = "Max cycles to return (default 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LockOrderGraphParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max call hops for interprocedural lock inlining (default 5)")]
    pub max_call_depth: Option<u32>,
    #[schemars(description = "Drop edges below this resource-key confidence (default 0.3)")]
    pub confidence_floor: Option<f32>,
    #[schemars(description = "Restrict to the in/out neighborhood of this lock resource_key")]
    pub resource_key: Option<String>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RuntimeDeadlockReconcileParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "The runtime trace text captured by the agent (off-CPU folded stack, perf script dump, or gdb backtrace)"
    )]
    pub trace_text: String,
    #[schemars(description = "Trace format: offcpu_folded | perf_script | gdb_bt")]
    pub format: String,
    #[schemars(
        description = "Max call hops for the static interprocedural lock graph (default 5)"
    )]
    pub max_call_depth: Option<u32>,
    #[schemars(description = "Drop static edges below this resource-key confidence (default 0.3)")]
    pub confidence_floor: Option<f32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TraceMapToCodeParams {
    #[schemars(description = "Project name to resolve frames against")]
    pub project: String,
    #[schemars(
        description = "The backtrace text (gdb `bt`, off-CPU folded stack, or a newline/`;`-separated frame list)"
    )]
    pub backtrace: String,
    #[schemars(
        description = "Backtrace format: gdb_bt | folded | auto (default auto — sniff gdb `#N` frames vs `;`-folded vs newline list)"
    )]
    pub format: Option<String>,
    #[schemars(description = "Max frames to resolve (default 64, max 512)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SyncSkeletonParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Symbol id to inspect (preferred)")]
    pub symbol_id: Option<i64>,
    #[schemars(description = "File relative-path (with `name`, resolves a symbol id)")]
    pub file: Option<String>,
    #[schemars(description = "Symbol name (with `file`, resolves a symbol id)")]
    pub name: Option<String>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ChannelDeadlockParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max findings to return (default 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConcurrencyBottlenecksParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max rows per metric (default 20, clamp 1..200)")]
    pub top: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConcurrencyForecastParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "Metric: deadlock_cycle_count | channel_cycle_count | blocked_recv_count | max_lock_contention (default deadlock_cycle_count)"
    )]
    pub metric: Option<String>,
    #[schemars(description = "History window in days (default 90)")]
    pub days: Option<i32>,
    #[schemars(
        description = "Threshold to forecast crossing of (default 5; 10 for max_lock_contention)"
    )]
    pub threshold: Option<f64>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SendSyncViolationsParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max matches to return (default: 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct QuadraticLoopsParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max matches to return (default: 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MissingPreallocationParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max matches to return (default: 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BlockingInAsyncParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max matches to return (default: 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CloneDensityParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max files to return (default: 30)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct IoHotpathParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max files to return (default: 30)")]
    pub limit: Option<i32>,
}

// SOTA Phase 6 — security
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TaintAnalysisParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max findings (default: 30)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SecretDetectionParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Minimum Shannon entropy to flag a literal (default: 4.0)")]
    pub min_entropy: Option<f64>,
    #[schemars(description = "Max findings (default: 100)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CryptoMisuseParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max findings (default: 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UnsafeDeserializationParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max matches (default: 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InjectionCandidatesParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "\"sql\" / \"shell\" / \"all\" (default)")]
    pub kind: Option<String>,
    #[schemars(description = "Max matches (default: 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UnprotectedRoutesParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max matches (default: 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CveSupplyChainParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max dependencies to return (default: 200)")]
    pub limit: Option<i32>,
}

// SOTA Phase 7 — API / contract
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PublicApiSurfaceParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Language filter (e.g. \"rust\"); omit = all")]
    pub language: Option<String>,
    #[schemars(description = "\"summary\" (default) or \"full\"")]
    pub format: Option<String>,
    #[schemars(description = "Max symbols to return when format=\"full\" (default: 500)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SemverBreakAuditParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(
        description = "How many recent commits to scan for historical public surface (default: 50)"
    )]
    pub window_commits: Option<u32>,
    #[schemars(description = "Max findings (default: 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DeprecatedButUsedParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max symbols to return (default: 30)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ApiStabilityParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "How many recent commits to scan (default: 100)")]
    pub window_commits: Option<u32>,
    #[schemars(description = "Max symbols to return (default: 50)")]
    pub limit: Option<i32>,
}

// SOTA Phase 8 — ML / embedding-based
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LshCloneDetectionParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Approximate-cosine threshold (default: 0.85)")]
    pub min_similarity: Option<f64>,
    #[schemars(description = "Max pairs (default: 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SemanticDriftParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max files (default: 30)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EmbeddingOutliersParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Number of nearest neighbours (default: 20)")]
    pub k: Option<u32>,
    #[schemars(description = "LOF threshold (default: 1.5)")]
    pub threshold: Option<f64>,
    #[schemars(description = "Max outliers (default: 30)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MultiResolutionPagerankParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max files (default: 50)")]
    pub limit: Option<i32>,
}

// SOTA Phase 9 — data engineering
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MigrationSafetyParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max findings (default: 50)")]
    pub limit: Option<i32>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DeadColumnsParams {
    #[schemars(description = "Project name (required)")]
    pub project: String,
    #[schemars(description = "Max columns (default: 50)")]
    pub limit: Option<i32>,
}
