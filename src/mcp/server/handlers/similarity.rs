//! Cross-project similarity tool handlers.
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

#[rmcp::tool_router(router = router_similarity, vis = "pub(crate)")]
impl McpServer {
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
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "compare_files",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_compare_files::tool_compare_files(self.ctx(), params),
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
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "find_similar_modules",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_find_similar_modules::tool_find_similar_modules(
                self.ctx(),
                params,
            ),
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
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "find_duplicates",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_find_duplicates::tool_find_duplicates(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Generate an actionable refactoring report identifying code that could be extracted into shared libraries. Builds on find_duplicates clustering with richer analysis: suggests crate names from common path segments, estimates shared lines, and ranks by project_count * avg_similarity. Requires the similarity batch scan to have run at least once."
    )]
    async fn refactoring_report(
        &self,
        Parameters(params): Parameters<RefactoringReportParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "refactoring_report",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_refactoring_report::tool_refactoring_report(self.ctx(), params),
        )
        .await
    }
}
