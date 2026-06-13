//! Core (hybrid/summarize/orient), telemetry, mandate & memory parameter types (part A).
//!
//! Extracted verbatim from `server.rs` (B.2 god-file split). All structs
//! re-exported by `params/mod.rs` and, transitively, by `server.rs` so
//! `crate::mcp::server::<Name>Params` resolves for every tool body file.
#![allow(unused_imports)]

use super::*;
use rmcp::schemars;
use serde::Deserialize;

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
    /// Weight for the third RRF leg (WFST/HybridLM-rescored query).
    #[schemars(
        description = "Weight for the third RRF leg (WFST lattice + HybridLM-rescored query). \
                       Default 1.0. Set 0.0 to force the legacy 2-leg behavior. The third leg \
                       activates only when the per-project HybridLM model file exists at \
                       <data_dir>/hybrid_lm/<slug>-p<project_id>/model.bin (populated by the \
                       `ngram-lm-train` cron)."
    )]
    pub wfst_lm_weight: Option<f64>,
    /// Max per-token Damerau-Levenshtein distance for query rewriting.
    #[schemars(
        description = "Max per-token Damerau-Levenshtein distance used when generating \
                       candidates for the third-leg lattice. Default 2."
    )]
    pub max_query_edit_distance: Option<usize>,
    // Shadow-ASR facet filters (Pattern D), same semantics as semantic_search /
    // text_search / grep: post-filter fused hits by their enclosing symbol.
    #[schemars(
        description = "Shadow-ASR filter: restrict hits to chunks whose enclosing symbol's \
                       return_type_tags contains ALL of these tags. Optional."
    )]
    pub return_type_tags: Option<Vec<String>>,
    #[schemars(
        description = "Shadow-ASR filter: restrict hits to chunks whose enclosing symbol carries \
                       at least one of these effects (e.g. ['unsafe','may_panic']). Optional."
    )]
    pub effects: Option<Vec<String>>,
    #[schemars(
        description = "Shadow-ASR filter: restrict hits to chunks whose enclosing symbol kind \
                       matches (e.g. \"function\", \"trait\", \"class\"). Optional."
    )]
    pub scope_kind: Option<String>,
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
pub struct QualityReportParams {
    #[schemars(description = "Project name (e.g. 'f1r3node-rust', 'pgmcp')")]
    pub project: String,
    #[schemars(
        description = "Output format: markdown (default) | org | latex | html | text | json. 'json' emits the structured report (computed grades inlined) for automated tooling."
    )]
    pub format: Option<String>,
    #[schemars(description = "Include the enumerated findings sections (default true)")]
    pub include_findings: Option<bool>,
    #[schemars(
        description = "Minimum severity to display: low|medium|high|critical (default low)"
    )]
    pub min_severity: Option<String>,
    #[schemars(description = "Include RecommendedFix blocks per finding (default true)")]
    pub include_recommended_fixes: Option<bool>,
    #[schemars(
        description = "Return a JSON envelope {rendered, report} so callers get both the rendered text and the structured report (default false)"
    )]
    pub include_underlying_json: Option<bool>,
    #[schemars(description = "Recent GPA history points in the trend strip (default 12, 0 = off)")]
    pub trend_points: Option<usize>,
    #[schemars(
        description = "Cron job names to force-refresh before aggregating (e.g. [\"symbol-extraction\",\"call-graph\",\"function-metrics\"]); default none"
    )]
    pub refresh_crons: Option<Vec<String>>,
}

// === Phase 1 (trends & forecasting): trajectory tool parameter types ===

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct QualityTrendParams {
    #[schemars(description = "Project name (as shown by list_projects)")]
    pub project: String,
    #[schemars(
        description = "Lookback window in days over `quality_report_history` (default 90, \
                       clamped 1..=3650). The cron snapshots GPAs every 6h, so 90d ≈ \
                       360 points."
    )]
    pub days: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct QualityForecastParams {
    #[schemars(description = "Project name (as shown by list_projects)")]
    pub project: String,
    #[schemars(
        description = "Lookback window in days used to fit the overall-GPA slope \
                       (default 90, clamped 1..=3650)."
    )]
    pub days: Option<i64>,
    #[schemars(
        description = "GPA threshold to project the crossing of (default 2.0 = the \
                       C-grade floor). The forecast reports how many weeks until the \
                       overall GPA, on its current slope, reaches this value."
    )]
    pub threshold: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadFileParams {
    #[schemars(description = "Absolute path of the file to read")]
    pub path: String,
    #[schemars(description = "1-based inclusive start line for a region read. \
                       Combine with `end_line` to fetch only a slice of the \
                       file (stitched from indexed chunks). Use this for long \
                       documents to avoid pulling 20–50k tokens for a paragraph.")]
    pub start_line: Option<i32>,
    #[schemars(
        description = "1-based inclusive end line for a region read. Pair with \
                       `start_line`."
    )]
    pub end_line: Option<i32>,
    #[schemars(description = "Inclusive chunk_index lower bound for a chunk-indexed \
                       region read. Useful when paging large documents.")]
    pub chunk_index_start: Option<i32>,
    #[schemars(description = "Inclusive chunk_index upper bound.")]
    pub chunk_index_end: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProjectTreeParams {
    #[schemars(description = "Project name")]
    pub project: String,
    #[schemars(description = "Maximum directory depth (default: 5)")]
    pub depth: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReindexParams {
    #[schemars(
        description = "Optional: re-extract only files of this language (e.g. \"latex\"), \
                       preserving every other file's incremental size+mtime skip. Omit to \
                       clear and rebuild the entire index."
    )]
    pub language: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct OrientParams {
    #[schemars(description = "Project name (as shown by list_projects)")]
    pub project: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct McpToolTelemetryParams {
    #[schemars(
        description = "Filter to a specific MCP tool name (e.g. \"grep\", \"semantic_search\")."
    )]
    pub tool: Option<String>,
    #[schemars(
        description = "Filter to a specific MCP client name (e.g. \"claude-code\", \"cursor\"). Matched case-sensitively against the lowercased name stored in mcp_tool_calls."
    )]
    pub client_name: Option<String>,
    #[schemars(
        description = "Filter to calls that named this project as the `project` parameter."
    )]
    pub project: Option<String>,
    #[schemars(description = "Lookback window in minutes (default 60, max 44640 = 31 days).")]
    pub since_minutes: Option<i32>,
    #[schemars(description = "Result limit for `aggregation=\"raw\"` (default 100, max 1000).")]
    pub limit: Option<i32>,
    #[schemars(
        description = "Aggregation shape: one of `summary`, `top_tools`, `top_callers`, `top_projects`, `error_rate`, `histogram`, `output_bytes` (top tools by serialized result size — the result-payload-slimming targeting view), `raw`. Default `summary`."
    )]
    pub aggregation: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AdoptionReportParams {
    #[schemars(
        description = "Lookback window in minutes (default 43200 = 30 days, max 44640 = 31 days)."
    )]
    pub since_minutes: Option<i32>,
    #[schemars(description = "Output format: json (default) | markdown.")]
    pub format: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MandateContextParams {
    #[schemars(
        description = "Project name (as shown by list_projects). Takes precedence over cwd."
    )]
    pub project: Option<String>,
    #[schemars(description = "Working directory used to resolve the nearest indexed project.")]
    pub cwd: Option<String>,
    #[schemars(
        description = "Session UUID. If supplied, response includes active session mandates and any promoted durable mandates for the resolved project."
    )]
    pub session_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SessionMandatesParams {
    #[schemars(description = "Session UUID. Either session_id or cwd must be supplied.")]
    pub session_id: Option<String>,
    #[schemars(
        description = "Working directory; returns mandates from any session matching this cwd."
    )]
    pub cwd: Option<String>,
    #[schemars(description = "Status filter: 'active' (default), 'all', 'promoted', 'retired'.")]
    pub status: Option<String>,
    #[schemars(description = "Max rows (1..=100, default 20).")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PromoteSessionMandateParams {
    #[schemars(description = "session_mandates.id of the row to promote.")]
    pub mandate_id: i64,
    #[schemars(
        description = "Target scope: 'project' (per-project rule) or 'workspace' (cross-project)."
    )]
    pub scope: String,
    #[schemars(
        description = "Project id to attach the promoted mandate to. Required when scope='project'."
    )]
    pub project_id: Option<i32>,
    #[schemars(
        description = "If true, also append the imperative under a marker section in the appropriate CLAUDE.md / AGENTS.md / .pgmcp.toml. Default false (DB-only)."
    )]
    pub write_to_file: Option<bool>,
    #[schemars(
        description = "Optional explicit file path to write to. If omitted, the handler picks CLAUDE.md / AGENTS.md per scope."
    )]
    pub target_file: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FileInfoParams {
    #[schemars(description = "Absolute path of the file")]
    pub path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ActiveClientsParams {
    #[serde(default)]
    #[schemars(
        description = "Optional project-name filter (as shown by list_projects); omit to list clients across all projects."
    )]
    pub project: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Also include recently-exited clients (default false → only currently-alive clients)."
    )]
    pub include_exited: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ClientProjectMatrixParams {
    #[serde(default)]
    #[schemars(
        description = "Optional project-name filter (as shown by list_projects); omit for all projects."
    )]
    pub project: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Lookback window in minutes (default 1440 = 24h; clamped to 1..=44640 = 31d)."
    )]
    pub since_minutes: Option<i32>,
    #[serde(default)]
    #[schemars(
        description = "How many recently-edited files to list per project (default 5; clamped 0..=50)."
    )]
    pub top_files_per_project: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProjectDependentsParams {
    #[schemars(
        description = "Project name (as shown by list_projects) — returns the projects that depend ON it."
    )]
    pub project: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProjectDependenciesParams {
    #[schemars(
        description = "Project name (as shown by list_projects) — returns the projects IT depends on."
    )]
    pub project: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CoordinateDependencyBlockParams {
    #[schemars(
        description = "The dependency project name (from list_projects) your build broke on."
    )]
    pub dependency: String,
    #[serde(default)]
    #[schemars(
        description = "Optional compiler-error excerpt naming the breakage (ground truth)."
    )]
    pub error_excerpt: Option<String>,
    #[serde(default)]
    #[schemars(description = "Your own project name (the blocked dependent), if known.")]
    pub dependent_project: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Your mcp_session_id; auto-filled with the caller's session over MCP."
    )]
    pub requester_session: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Optional: the public_id of YOUR work-item that this dependency blocks. It is \
                       set `blocked` now and auto-unblocked (blocked → ready, by the git-scanner \
                       gatekeeper — never by the editor) when the dependency is restored."
    )]
    pub blocked_work_item: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CoordinationRespondParams {
    #[schemars(description = "The coordination request id (from coordinate_dependency_block).")]
    pub request_id: i64,
    #[schemars(description = "Your response: accept | decline | moved.")]
    pub response: String,
    #[serde(default)]
    #[schemars(description = "On 'moved', the worktree branch you moved your in-flight edits to.")]
    pub worktree_branch: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Your mcp_session_id; auto-filled with the caller's session over MCP."
    )]
    pub editor_session: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SuggestWorktreeParams {
    #[schemars(
        description = "The project name to suggest a worktree-move for (the dependency being edited)."
    )]
    pub project: String,
    #[serde(default)]
    #[schemars(description = "Feature branch name for the moved work (default 'wip').")]
    pub feature_branch: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RecallPromptsParams {
    #[schemars(
        description = "Free-text query — embedded and matched by cosine similarity \
                       against historical prompts in `session_prompts`."
    )]
    pub query: String,
    #[schemars(description = "Optional project filter (matches `projects.name`).")]
    pub project: Option<String>,
    #[schemars(description = "Optional session UUID filter.")]
    pub session: Option<String>,
    #[schemars(description = "Max rows (1..=200, default 10).")]
    pub limit: Option<i32>,
}

// ----------------------------------------------------------------------------
// Memory-server Phase 3.1: official MCP memory-server compatible CRUD Params
// ----------------------------------------------------------------------------

/// Shared scope-filter object accepted by every `memory_*` tool. Each
/// field is optional; missing fields resolve to NULL ("any") on the
/// `memory_scope` row.
#[derive(Debug, Clone, Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct MemoryScopeParam {
    pub user_id: Option<String>,
    pub agent_id: Option<String>,
    /// Optional session UUID (string-encoded).
    pub session_id: Option<String>,
    pub project_id: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryEntityInput {
    #[schemars(description = "Entity name (the unique identifier used by the official server).")]
    pub name: String,
    #[schemars(
        description = "Entity type (free-form string, e.g. 'person', 'project', 'concept')."
    )]
    pub entity_type: String,
    #[schemars(description = "Initial observations attached at create-time. Optional.")]
    pub observations: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryCreateEntitiesParams {
    #[schemars(description = "Entities to create or extend. Idempotent on (name, entity_type).")]
    pub entities: Vec<MemoryEntityInput>,
    #[schemars(
        description = "Scope under which to attach the entities. Defaults to workspace-wide."
    )]
    pub scope: Option<MemoryScopeParam>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryRelationInput {
    pub from: String,
    pub to: String,
    pub relation_type: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryCreateRelationsParams {
    #[schemars(description = "Directed relations between entities. Endpoints must already exist.")]
    pub relations: Vec<MemoryRelationInput>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryObservationInput {
    pub entity_name: String,
    pub contents: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryAddObservationsParams {
    pub observations: Vec<MemoryObservationInput>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryDeleteEntitiesParams {
    pub names: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryObservationDeletionInput {
    pub entity_name: String,
    pub observations: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryDeleteObservationsParams {
    pub deletions: Vec<MemoryObservationDeletionInput>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryDeleteRelationsParams {
    pub relations: Vec<MemoryRelationInput>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryReadGraphParams {
    pub scope: Option<MemoryScopeParam>,
    #[schemars(description = "Max entities returned (default 200, max 2000).")]
    pub limit_entities: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemorySearchNodesParams {
    #[schemars(
        description = "Substring matched against entity name/type/canonical_name and observation content (ILIKE)."
    )]
    pub query: String,
    pub scope: Option<MemoryScopeParam>,
    #[schemars(description = "Max rows (1..=500, default 20).")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryOpenNodesParams {
    pub names: Vec<String>,
    #[serde(default)]
    #[schemars(
        description = "Optional project name (list_projects) to scope the effect_breakdown channel; omit for an empty breakdown."
    )]
    pub project: Option<String>,
}

// ----------------------------------------------------------------------------
// Phase 3.2 pgmcp extensions
// ----------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemorySemanticSearchParams {
    pub query: String,
    pub scope: Option<MemoryScopeParam>,
    #[schemars(
        description = "Optional cognitive-tier filter: working | episodic | semantic | procedural | reflective."
    )]
    pub tier: Option<String>,
    #[schemars(description = "Max rows (1..=200, default 20).")]
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryHybridSearchParams {
    pub query: String,
    pub scope: Option<MemoryScopeParam>,
    pub tier: Option<String>,
    pub limit: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryFactsAtParams {
    #[schemars(description = "RFC3339 timestamp. Defaults to NOW().")]
    pub as_of: Option<String>,
    pub scope: Option<MemoryScopeParam>,
    pub tier: Option<String>,
    pub limit_entities: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryRelationsTraverseParams {
    pub seed_entity_ids: Vec<i64>,
    #[schemars(description = "BFS depth cap (1..=6, default 2).")]
    pub max_depth: Option<i32>,
    #[schemars(description = "Restrict expansion to one relation_type. Optional.")]
    pub relation_filter: Option<String>,
    #[schemars(description = "Hard cap on total nodes returned (default 200, max 1000).")]
    pub max_nodes: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryAnchorEntityParams {
    pub entity_id: i64,
    pub file_id: Option<i64>,
    pub chunk_id: Option<i64>,
    pub topic_id: Option<i64>,
    #[schemars(description = "Anchor to a file_symbols.id (unified-graph symbol node).")]
    pub symbol_id: Option<i64>,
    #[schemars(description = "Anchor to a projects.id (unified-graph project node).")]
    pub project_id: Option<i32>,
    pub anchor_type: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryUnanchorEntityParams {
    pub anchor_id: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryFindCodeForEntityParams {
    pub entity_id: i64,
    pub anchor_type: Option<String>,
}
