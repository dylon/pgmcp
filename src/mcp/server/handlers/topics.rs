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
}
