//! SOTA API & semver-contract handlers.
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

#[rmcp::tool_router(router = router_api_contract, vis = "pub(crate)")]
impl McpServer {
    #[tool(
        description = "Enumerate public symbols from `file_symbols.visibility='public'`. Per-kind counts (default) or full list."
    )]
    async fn public_api_surface(
        &self,
        Parameters(params): Parameters<PublicApiSurfaceParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "public_api_surface",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_public_api_surface::tool_public_api_surface(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Semver-break audit: public symbols seen in recent git history but missing from the current public API. Likely renames flagged by Levenshtein."
    )]
    async fn semver_break_audit(
        &self,
        Parameters(params): Parameters<SemverBreakAuditParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "semver_break_audit",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_semver_break_audit::tool_semver_break_audit(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Symbols annotated as deprecated but still called from inside the project. Migrate then delete."
    )]
    async fn deprecated_but_used(
        &self,
        Parameters(params): Parameters<DeprecatedButUsedParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "deprecated_but_used",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_deprecated_but_used::tool_deprecated_but_used(
                self.ctx(),
                params,
            ),
        )
        .await
    }
    #[tool(
        description = "API stability score per public symbol (Bogart EMSE 2016) — change-rate over recent commits."
    )]
    async fn api_stability(
        &self,
        Parameters(params): Parameters<ApiStabilityParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "api_stability",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_api_stability::tool_api_stability(self.ctx(), params),
        )
        .await
    }
}
