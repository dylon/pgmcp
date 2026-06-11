//! Recommendation & refactoring-action tool handlers.
//!
//! Tool handlers extracted verbatim from `server.rs` (B.3 god-file split).
//! Only the relative `super::tools::` path was rewritten to the absolute
//! `crate::mcp::tools::`; bodies are otherwise identical. The per-block
//! router is composed in `server.rs` via `assembled_tool_router()`.
#![allow(clippy::too_many_lines)]

use crate::mcp::server::McpServer;
use crate::mcp::server::*;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer};

#[rmcp::tool_router(router = router_recommend, vis = "pub(crate)")]
impl McpServer {
    #[tool(
        description = "Cluster near-duplicate code chunks (not files) across projects and propose a \
shared function name per cluster. \
USE WHEN: looking for fine-grained DRY opportunities — two files might be 90% different but share \
a small embedded utility worth extracting. Distinct from `find_duplicates` (file-level) and \
`refactoring_report` (whole-crate extraction). \
DO NOT USE WHEN: you want library-extraction candidates (use `refactoring_report`) or you have \
a specific seed file (use `find_similar_modules`). \
Each cluster includes a typed `recommended_fix` (extract_function or extract_module) with \
proposed function name, module name, and priority_score = loc_avg × project_count × (chunk_count - 1). \
Reads the materialized similarity table; requires the 6-hour similarity-scan cron to have run."
    )]
    async fn chunk_clusters(
        &self,
        Parameters(params): Parameters<ChunkClustersParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "chunk_clusters",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_chunk_clusters::tool_chunk_clusters(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "DRY within a single file: find intra-file chunk pairs above a similarity \
threshold and propose private-helper extractions. \
USE WHEN: you've opened a file and want to know whether parts of it are repeating themselves — \
e.g. multiple HTTP handlers building the same request envelope. Real-time over the indexed \
chunks; no cron dependency. \
DO NOT USE WHEN: looking for cross-file or cross-project DRY (use `chunk_clusters`). \
Returns clusters of similar chunks, each with a proposed `extract_function` recommended_fix \
(action=extract_function, suggested_name, line ranges). Pass `file` as `project:relative_path` \
or absolute path."
    )]
    async fn internal_dry(
        &self,
        Parameters(params): Parameters<InternalDryParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "internal_dry",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_internal_dry::tool_internal_dry(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Extraction-to-shared-crate candidates with effort + risk + proposed API surface. \
Strict superset of `refactoring_report`. \
USE WHEN: planning a `extract_module` PR — you want to know not just *which* code to extract, but \
*how big* the migration is (loc_to_extract, call_sites_to_update) and *how risky* (high churn? many \
unresolved imports?). Each candidate carries a typed `recommended_fix(action=extract_module)`. \
DO NOT USE WHEN: doing a quick \"what duplicates exist?\" survey — `find_duplicates` or \
`refactoring_report` is faster. \
Reads materialized similarity table; requires the 6-hour similarity-scan cron."
    )]
    async fn extraction_candidates(
        &self,
        Parameters(params): Parameters<ExtractionCandidatesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "extraction_candidates",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_extraction_candidates::tool_extraction_candidates(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Trait / interface / Protocol extraction candidates: chunks at *medium* \
similarity (0.70-0.85) sharing the same topic — different implementations of the same idea. \
USE WHEN: looking for OOP / Rust-trait abstraction opportunities. Distinct from `chunk_clusters` \
(near-duplicates → extract function) and `extraction_candidates` (whole-file → extract crate). \
DO NOT USE WHEN: chunks are nearly identical (use `chunk_clusters`) or you have no topic data \
yet (run `discover_topics` first). \
Each candidate includes a typed `recommended_fix(action=extract_trait|extract_interface)` with \
proposed method name, abstraction kind by language, and a diversity-rewarded priority score \
(higher reward for less-similar implementations of the same topic)."
    )]
    async fn pattern_abstraction_candidates(
        &self,
        Parameters(params): Parameters<PatternAbstractionParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "pattern_abstraction_candidates",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_pattern_abstraction::tool_pattern_abstraction(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Codegen-worthy near-identical chunks: clusters where chunks differ only by \
renamed identifiers — strong macro / generic / template candidates. \
USE WHEN: auditing for boilerplate that should be a `macro_rules!` (Rust), generic (TS/Java), \
or parametric template. Aggressive default threshold (0.96) so only near-identical code surfaces. \
DO NOT USE WHEN: looking for general DRY (use `chunk_clusters`); a 0.88 cluster of \"similar idea\" \
code is not a boilerplate cluster. \
For each cluster, identifiers are normalized to positional placeholders; the differing values are \
reported (so you know which identifiers vary across instances). Recommended fix is always \
`extract_macro`. Reads materialized similarity table; requires the 6-hour similarity-scan cron."
    )]
    async fn boilerplate_clusters(
        &self,
        Parameters(params): Parameters<BoilerplateClustersParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "boilerplate_clusters",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_boilerplate_clusters::tool_boilerplate_clusters(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Likely-dead files based on graph + history evidence: low PageRank percentile \
(bottom 25%), in_degree <= 1, and idle for >= 540 days by default. \
USE WHEN: cleaning up legacy modules during a quarterly audit. Distinct from `find_orphans` \
(which uses topic membership) — this combines graph centrality, importer count, and authorial \
abandonment. \
DO NOT USE WHEN: file_metrics is empty (graph cron hasn't run) — the tool soft-fails with a \
guidance message. \
For files with `in_degree=0`, the recommended_fix is `delete_file`. For `in_degree=1`, the fix \
is `move_function` (relocate the single referenced symbol into its sole importer, then delete)."
    )]
    async fn stale_zombie_detector(
        &self,
        Parameters(params): Parameters<StaleZombieParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "stale_zombie_detector",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_stale_zombie::tool_stale_zombie(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "For each god file (line_count >= 500 by default), propose an explicit split \
along FCM topic boundaries with line ranges and a typed `recommended_fix(action=split_file)`. \
USE WHEN: an `architecture_violations` god_module finding or a `design_smell_detection` god_class \
finding has surfaced — this tool turns the diagnosis into a concrete sub-file proposal with chunk \
ranges and per-piece suggested filenames. \
DO NOT USE WHEN: no FCM topics have been computed yet (run `discover_topics` first; otherwise \
this tool soft-fails with `health.topics_present:false`). \
Single-topic god files get an `add_test` recommendation instead — they're cohesive and shouldn't \
be split."
    )]
    async fn recommend_module_split(
        &self,
        Parameters(params): Parameters<RecommendModuleSplitParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "recommend_module_split",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_recommend_module_split::tool_recommend_module_split(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "For each Tarjan SCC (cycle group) in a project's import graph, recommend a \
specific edge to break and the strategy: `extract_interface` or `invert_dependency`. \
USE WHEN: `circular_dependencies` has surfaced cycles and you want explicit, agent-executable fix \
guidance — which edge to flip, which side gets the new abstraction, which imports must update. \
DO NOT USE WHEN: the import graph is empty (graph cron hasn't run); soft-fails with \
`health.graph_stale:true`. \
Strategy heuristic: when one cycle endpoint is more abstract / stable, the edge from the \
less-abstract side becomes a trait/interface dependency on the abstract side (`invert_dependency`); \
otherwise, propose extracting a new shared interface for the lower-coupling endpoint."
    )]
    async fn fix_circular_dependency(
        &self,
        Parameters(params): Parameters<FixCircularDependencyParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "fix_circular_dependency",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_fix_circular_dependency::tool_fix_circular_dependency(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "For each shotgun-surgery smell hub (file co-changing with many partners), \
pick the absorbing centroid file and recommend consolidation. \
USE WHEN: a `design_smell_detection` shotgun_surgery finding has surfaced — turn the \"this hub \
ripples to N partners\" signal into a typed `recommended_fix(action=consolidate_logic)` with the \
target file and per-partner moves enumerated. \
DO NOT USE WHEN: git history is disabled for the project (no co-change data); soft-fails with \
`health.git_history_present:false`. \
Centroid heuristic: among the hub plus its co-change partners, pick the file with the highest \
PageRank — the most architecturally central place to consolidate the scattered logic."
    )]
    async fn shotgun_surgery_fix(
        &self,
        Parameters(params): Parameters<ShotgunSurgeryFixParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "shotgun_surgery_fix",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_shotgun_surgery_fix::tool_shotgun_surgery_fix(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Infer a layered architecture for a project (Louvain on imports + SDP-based \
layer assignment) and list every cross-layer import as a violation, each with a typed \
`recommended_fix`. \
USE WHEN: doing an architecture audit and you want a layered view *plus* the violations that \
break it — UI files reaching directly into data layer, deep upward dependencies, etc. \
DO NOT USE WHEN: the project's import graph is small (< num_layers communities) — the heuristic \
collapses and confidence drops sharply. The default web-biased layer-naming is unreliable for \
non-web codebases; override via `layer_names`. \
Per-violation fix dispatch: skip-N-layer downward → add_anti_corruption_layer; small leaf → \
move_function; upward → invert_dependency."
    )]
    async fn recommend_layering(
        &self,
        Parameters(params): Parameters<RecommendLayeringParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "recommend_layering",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_recommend_layering::tool_recommend_layering(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Given a starter file, recommend the right PR scope: minimum (direct \
importers), recommended (+ co-change Jaccard ≥ threshold), maximum (+ depth-N reverse BFS + \
topic neighbors). Emits a `verdict`: focused / normal / sprawling. \
USE WHEN: about to open a PR and want to know whether other files should travel with it. \
DO NOT USE WHEN: git history is disabled — co-change leg drops out and the recommendation \
quality declines (still works on imports + topics)."
    )]
    async fn pr_scope_recommender(
        &self,
        Parameters(params): Parameters<PrScopeRecommenderParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "pr_scope_recommender",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_pr_scope::tool_pr_scope(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Files in the intersection of top-P% PageRank, top-P% churn, and top-P% \
fix_commit_ratio — the most fragile critical paths. \
USE WHEN: deciding where to invest test/docs effort, or as a release-readiness audit (\"what's \
the most expensive risk we're shipping?\"). \
DO NOT USE WHEN: file_metrics or git history is empty — the percentile gates collapse to zero \
and the result is empty. Each row carries a `priority` (P0/P1/P2) and an action recommendation \
(add integration test, freeze API, refactor)."
    )]
    async fn hot_path_audit(
        &self,
        Parameters(params): Parameters<HotPathAuditParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "hot_path_audit",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_hot_path_audit::tool_hot_path_audit(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Per-file knowledge-concentration risk: top author's share of blamed lines × \
PageRank ÷ distinct authors. Surfaces files where a single contributor's departure causes \
maximum harm. \
USE WHEN: planning team coverage / PTO, or auditing a release candidate for fragility. \
DO NOT USE WHEN: file_chunks blame columns are empty (project hasn't run the git-blame cron). \
Returns critical / warning / healthy buckets and a `bus_factor_estimate` (greedy set-cover ≥50% of total blamed lines)."
    )]
    async fn bus_factor_map(
        &self,
        Parameters(params): Parameters<BusFactorMapParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "bus_factor_map",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_bus_factor_map::tool_bus_factor_map(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Given a list of changed files, rank reviewers by recent ownership and \
suggest a minimum cover-set (≥80% of files with the fewest reviewers). \
USE WHEN: about to open a PR and need to pick reviewers — pastes the file list, gets a \
ranked author list with per-file breakdowns. \
DO NOT USE WHEN: blame columns are empty — files with no blame data appear in `unowned_files`. \
Pass the PR author's email in `exclude_authors` to skip self-review."
    )]
    async fn reviewer_recommender(
        &self,
        Parameters(params): Parameters<ReviewerRecommenderParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "reviewer_recommender",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_reviewer_recommender::tool_reviewer_recommender(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Audit external/unresolved import targets across one or all projects. \
USE WHEN: doing a quarterly dep audit — surface third-party deps, rank by usage centrality + \
staleness, recommend prune / upgrade / consolidate / keep. \
DO NOT USE WHEN: code_graph_edges has no unresolved-target rows."
    )]
    async fn dependency_health(
        &self,
        Parameters(params): Parameters<DependencyHealthParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "dependency_health",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_dependency_health::tool_dependency_health(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Snippet-as-query: embed a code snippet and find the closest implementations \
across all indexed projects, plus a `verdict` (reuse / adapt / new). \
USE WHEN: mid-implementation, you want to know whether anyone in the workspace is already \
solving this. Distinct from `semantic_search` (which targets natural-language queries). \
DO NOT USE WHEN: you have a known seed file — use `find_similar_modules`."
    )]
    async fn pattern_search(
        &self,
        Parameters(params): Parameters<PatternSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "pattern_search",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_pattern_search::tool_pattern_search(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Score files by likelihood of conflicting with peer in-flight work, using \
overlapping recent commits as a proxy. \
USE WHEN: about to land a long-lived feature branch and want to know which files are also \
being edited concurrently. \
DO NOT USE WHEN: git history is disabled — soft-fails with `health.git_history_present:false`."
    )]
    async fn merge_conflict_risk(
        &self,
        Parameters(params): Parameters<MergeConflictRiskParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "merge_conflict_risk",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_merge_conflict_risk::tool_merge_conflict_risk(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Surface symbols whose naming convention diverges from the dominant \
convention within their directory. \
USE WHEN: enforcing or auditing per-module naming consistency. \
DO NOT USE WHEN: file_symbols data is absent — this tool requires the Tier-0e tree-sitter pass. \
Today, soft-fails with `health.symbols_present:false` and a guidance message; once Phase 0b \
ships, returns `divergences[]` with `recommended_fix(action=move_function)`."
    )]
    async fn naming_consistency(
        &self,
        Parameters(params): Parameters<NamingConsistencyParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "naming_consistency",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_naming_consistency::tool_naming_consistency(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Flag imports embedded inside function / method bodies instead of at the top \
of the file or module (imports at a `mod tests { … }` top are fine). Imports belong at the top of \
their scope; nested ones hide a scope's real dependencies and breed duplicated `use` lines. \
USE WHEN: auditing import hygiene or hunting duplicated imports across a project. \
DO NOT USE WHEN: file_symbols data is absent — requires the Tier-0e symbol-extraction pass; \
soft-fails with `health.symbols_present:false`. Returns `violations[]` (file, line, import, \
enclosing symbol, per-import duplication count) plus a `by_file` rollup."
    )]
    async fn import_hygiene(
        &self,
        Parameters(params): Parameters<ImportHygieneParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "import_hygiene",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_import_hygiene::tool_import_hygiene(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Project- or file-level growth trajectory over time: commits, authors, and \
optionally LOC per bucket (week/month/quarter). \
USE WHEN: investigating whether a module is growing fast enough to need a preemptive split, or \
auditing release-velocity trends. \
DO NOT USE WHEN: git history is disabled or the lookback window has < 4 buckets — trend math \
falls back to raw bucket data with no projection."
    )]
    async fn module_growth_trajectory(
        &self,
        Parameters(params): Parameters<ModuleGrowthParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "module_growth_trajectory",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_module_growth::tool_module_growth(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Given a 'modern' reference file, find legacy/older usages of similar \
patterns across the corpus. \
USE WHEN: you've just rewritten a feature and want to know where the old version is still in \
use, so you can plan migrations. \
DO NOT USE WHEN: no chunks found for the reference."
    )]
    async fn adoption_lag(
        &self,
        Parameters(params): Parameters<AdoptionLagParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "adoption_lag",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_adoption_lag::tool_adoption_lag(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Compose a phased remediation plan: aggregate `recommended_fix` items from \
bug_prediction, technical_debt_analysis, architecture_violations, design_smell_detection, \
stale_zombie_detector, and fix_circular_dependency. Rank by cost-benefit and bin-pack into \
'now' / 'next' / 'later' for the requested time_horizon. \
USE WHEN: planning a remediation sprint — one ranked, time-budgeted list across every quality \
dimension instead of running 6 tools separately."
    )]
    async fn tech_debt_burn_down(
        &self,
        Parameters(params): Parameters<TechDebtBurnDownParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "tech_debt_burn_down",
            45,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_tech_debt_burn_down::tool_tech_debt_burn_down(
                self.ctx(),
                params,
            ),
        )
        .await
    }
}
