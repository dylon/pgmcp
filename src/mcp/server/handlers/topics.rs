//! Topic-discovery & document-analysis handlers.
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

#[rmcp::tool_router(router = router_topics, vis = "pub(crate)")]
impl McpServer {
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
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "discover_topics",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_discover_topics::tool_discover_topics(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Cross-project topic redundancy (ADR-029): GLOBAL topics whose chunks span \
multiple projects — shared concerns / fork-redundancy / consolidation candidates, ranked by spread \
then size. USE WHEN you want to find duplicated functionality or shared themes across projects. \
Reads the global topic model (run topic-clustering cron if empty). Returns {count, shared_topics[]}."
    )]
    async fn cross_project_topic_redundancy(
        &self,
        Parameters(params): Parameters<CrossProjectTopicRedundancyParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "cross_project_topic_redundancy",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_cross_project_topic_redundancy::tool_cross_project_topic_redundancy(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Meta-clustering hierarchy over global topic centroids (Phase 9). Returns FCM-based meta-groups where each meta-group's parent_topic_ids point to the global topics it contains. Complementary view to discover_topics — chunk-to-global-topic assignments remain authoritative for cross-document comparability."
    )]
    async fn topic_hierarchy_fcm(
        &self,
        Parameters(params): Parameters<TopicHierarchyFcmParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "topic_hierarchy_fcm",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_topic_hierarchy_fcm::tool_topic_hierarchy_fcm(
                self.ctx(),
                params,
            ),
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
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "find_orphans",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_find_orphans::tool_find_orphans(self.ctx(), params),
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
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "find_misplaced_code",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_find_misplaced_code::tool_find_misplaced_code(
                self.ctx(),
                params,
            ),
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
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "find_coupled_files",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_find_coupled_files::tool_find_coupled_files(self.ctx(), params),
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
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let project_hint = Some(params.project.clone());
        instrumented_tool_wrap_with_project(
            self.stats(),
            "test_coverage_gaps",
            30,
            &_ctx,
            &summarize_debug(&params),
            project_hint,
            crate::mcp::tools::tool_test_coverage_gaps::tool_test_coverage_gaps(self.ctx(), params),
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
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let project = params.project.clone();
        instrumented_tool_wrap_with_project(
            self.stats(),
            "complexity_hotspots",
            30,
            &_ctx,
            &summarize_debug(&params),
            Some(project),
            crate::mcp::tools::tool_complexity_hotspots::tool_complexity_hotspots(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Show how discovered topics relate hierarchically using agglomerative clustering on topic centroids. Reveals module boundaries and related topic groups. Groups with low merge distance contain highly related topics that could be combined."
    )]
    async fn topic_hierarchy(
        &self,
        Parameters(params): Parameters<TopicHierarchyParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "topic_hierarchy",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_topic_hierarchy::tool_topic_hierarchy(self.ctx(), params),
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
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "suggest_merges",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_suggest_merges::tool_suggest_merges(self.ctx(), params),
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
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "suggest_splits",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_suggest_splits::tool_suggest_splits(self.ctx(), params),
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
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "doc_coverage_gaps",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_doc_coverage_gaps::tool_doc_coverage_gaps(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Per-project topic fingerprint: dominant topics, a specialization index \
(normalized entropy + Gini — focused vs sprawling), and stored coherence. \
USE WHEN: you want to know what a project is about and how focused it is, or to compare all \
projects' specialization at a glance. \
DO NOT USE WHEN: you want which file is mislabeled (find_misplaced_code) or the raw topic list \
(discover_topics). \
With `project`: one fingerprint. Without: a comparison table over all projects. Requires \
discover_topics (topic assignments) first."
    )]
    async fn project_topic_profile(
        &self,
        Parameters(params): Parameters<ProjectTopicProfileParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "project_topic_profile",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_project_topic_profile::tool_project_topic_profile(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Cross-project theme overlap from the global roll-up: which themes are \
shared substrate (span many projects) vs project-specific, plus each project's shared/unique split. \
USE WHEN: mapping which concerns are common infrastructure across the workspace vs siloed. \
DO NOT USE WHEN: you want a single project's topics (discover_topics / project_topic_profile). \
Requires a global discover_topics roll-up first."
    )]
    async fn topic_project_map(
        &self,
        Parameters(params): Parameters<TopicProjectMapParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "topic_project_map",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_topic_project_map::tool_topic_project_map(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Cluster projects by topic similarity and flag likely redundant forks / \
backups (.bak copies, PR-branch clones, rewrite families). \
USE WHEN: auditing a multi-project workspace for duplication / near-duplicate forks, or finding \
which projects are most alike. \
DO NOT USE WHEN: you want within-project duplicate code (find_duplicates / lsh_clone_detection). \
method=centroid (default) compares aggregated topic centroids; global_jsd compares global-theme \
distributions. Requires topic assignments (discover_topics) first."
    )]
    async fn project_topic_similarity(
        &self,
        Parameters(params): Parameters<ProjectTopicSimilarityParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "project_topic_similarity",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_project_topic_similarity::tool_project_topic_similarity(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Topic-topic coupling graph for a project: communities of entangled topics \
(Louvain over chunks co-assigned to multiple topics) + bridge topics that span communities \
(cross-cutting concerns). \
USE WHEN: finding which concerns are tangled together or which topics cut across the codebase. \
DO NOT USE WHEN: you want file-level coupling (find_coupled_files) or import cycles \
(circular_dependencies). Requires topic assignments (discover_topics) first."
    )]
    async fn topic_cooccurrence(
        &self,
        Parameters(params): Parameters<TopicCooccurrenceParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "topic_cooccurrence",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_topic_cooccurrence::tool_topic_cooccurrence(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Where the topic model is weak: orphan (uncategorized) chunks, thin topics \
(too few chunks), and low-cohesion topics — per project. \
USE WHEN: auditing topic-model quality / coverage, or finding code that escaped clustering. \
DO NOT USE WHEN: you want docs-vs-code gaps (doc_coverage_gaps) or test gaps (test_coverage_gaps). \
With `project`: one project. Without: all projects. Requires discover_topics first."
    )]
    async fn topic_coverage_gaps(
        &self,
        Parameters(params): Parameters<TopicCoverageGapsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "topic_coverage_gaps",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_topic_coverage_gaps::tool_topic_coverage_gaps(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Per-topic ownership map from git blame: for each topic, the top authors, \
the bus factor (min authors owning ≥50% of the topic's code), and ownership concentration. \
USE WHEN: finding who knows a domain (the consensus / parser / semiring code), onboarding, or \
bus-factor risk by concern. \
DO NOT USE WHEN: you want per-file ownership (bus_factor_map / knowledge_silos). Requires git \
blame indexing + discover_topics."
    )]
    async fn topic_owners(
        &self,
        Parameters(params): Parameters<TopicOwnersParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "topic_owners",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_topic_owners::tool_topic_owners(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Emerging vs declining topics over time + the quality trajectory. \
USE WHEN: you want to know which themes are rising or fading, or whether topic-model quality is \
trending up/down. \
DO NOT USE WHEN: you want the current snapshot (discover_topics / project_topic_profile). \
mode=longitudinal (default) uses the topics-size-history series (needs ≥2 snapshots); mode=quality \
forecasts the aggregate coherence/diversity metrics; mode=chunk_age is an immediate blame-date proxy."
    )]
    async fn topic_trends(
        &self,
        Parameters(params): Parameters<TopicTrendsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "topic_trends",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_topic_trends::tool_topic_trends(self.ctx(), params),
        )
        .await
    }
}
