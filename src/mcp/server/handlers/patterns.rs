//! Software-pattern catalog & commit-search handlers.
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

#[rmcp::tool_router(router = router_patterns, vis = "pub(crate)")]
impl McpServer {
    #[tool(description = "Semantic search over git commit messages and diffs. \
USE WHEN: investigating when a feature was added, when a bug was fixed, how a piece of \
code evolved, or who last touched a concept ('fix database timeout', 'add authentication'). \
DO NOT USE WHEN: you have an exact commit hash (`git show <hash>` is faster) or you only \
need recent commits in the current cwd (`git log` is faster). \
Requires per-project opt-in via [git] index_history = true in .pgmcp.toml.")]
    async fn search_commits(
        &self,
        Parameters(params): Parameters<SearchCommitsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap_with_project(
            self.stats(),
            "search_commits",
            30,
            &_ctx,
            &summarize_debug(&params),
            params.project.clone(),
            crate::mcp::tools::tool_search_commits::tool_search_commits(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Semantic search over the dedicated software pattern and anti-pattern knowledge index. \
USE WHEN: designing a feature/refactor and you want pattern candidates, anti-pattern warnings, or paradigm-specific design guidance. \
DO NOT USE WHEN: searching indexed source files — use semantic_search/hybrid_search for code. \
The pattern index is separate from file_chunks and includes locally imported full-text pattern documentation plus curated cards."
    )]
    async fn software_pattern_search(
        &self,
        Parameters(params): Parameters<SoftwarePatternSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "software_pattern_search",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_software_patterns::tool_software_pattern_search(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Recommend software design patterns and anti-patterns to avoid for a feature or refactor task. \
USE WHEN: drafting an implementation plan and selecting an approach for a target paradigm. \
Returns structured recommendations with source citations from the separate pattern knowledge index."
    )]
    async fn recommend_design_patterns(
        &self,
        Parameters(params): Parameters<RecommendDesignPatternsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "recommend_design_patterns",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_software_patterns::tool_recommend_design_patterns(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Review a proposed design against the software pattern knowledge index. \
USE WHEN: checking a plan for anti-pattern risks and better paradigm-specific alternatives before implementation."
    )]
    async fn review_design_patterns(
        &self,
        Parameters(params): Parameters<ReviewDesignPatternsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "review_design_patterns",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_software_patterns::tool_review_design_patterns(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Fetch a full software pattern or anti-pattern card by slug or id, with source links and optional excerpts."
    )]
    async fn get_software_pattern(
        &self,
        Parameters(params): Parameters<GetSoftwarePatternParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "get_software_pattern",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_software_patterns::tool_get_software_pattern(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "List software patterns and anti-patterns by paradigm, kind, category, or source family."
    )]
    async fn list_software_patterns(
        &self,
        Parameters(params): Parameters<ListSoftwarePatternsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "list_software_patterns",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_software_patterns::tool_list_software_patterns(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Pattern catalog statistics: paradigms, patterns, source families, chunks, and embedding status."
    )]
    async fn pattern_catalog_stats(
        &self,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "pattern_catalog_stats",
            30,
            &_ctx,
            "",
            crate::mcp::tools::tool_software_patterns::tool_pattern_catalog_stats(self.ctx()),
        )
        .await
    }

    #[tool(
        description = "Admin tool to seed, import, or re-embed the local full-text software pattern catalog. \
mode=seed_only embeds bundled cards; mode=source_family imports one source family; mode=all imports all registered source URLs."
    )]
    async fn refresh_pattern_catalog(
        &self,
        Parameters(params): Parameters<RefreshPatternCatalogParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        // mode=all touches ~50 registered source families; each fetches an
        // article body over HTTP and re-embeds 10-30 chunks. A 10-minute
        // ceiling accommodates that without leaving the call open forever.
        // Per-source progress is committed independently, so a timeout still
        // preserves what landed before the deadline.
        instrumented_tool_wrap(
            self.stats(),
            "refresh_pattern_catalog",
            600,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_software_patterns::tool_refresh_pattern_catalog(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Admin tool to attach full-text local documentation or snippets to an existing software pattern and embed them."
    )]
    async fn upsert_pattern_source(
        &self,
        Parameters(params): Parameters<UpsertPatternSourceParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        // Single-source manual ingestion: 5 minutes covers very large pasted
        // bodies (entire books, RFCs) and the per-chunk embedding loop.
        instrumented_tool_wrap(
            self.stats(),
            "upsert_pattern_source",
            300,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_software_patterns::tool_upsert_pattern_source(
                self.ctx(),
                params,
            ),
        )
        .await
    }
}
