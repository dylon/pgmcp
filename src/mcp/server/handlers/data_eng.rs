//! SOTA data-engineering handlers.
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

#[rmcp::tool_router(router = router_data_eng, vis = "pub(crate)")]
impl McpServer {
    #[tool(
        description = "Migration-safety audit (Strong-Migrations + Curino VLDB 2008): DROP/ALTER, non-CONCURRENT index, NOT NULL without default."
    )]
    async fn migration_safety(
        &self,
        Parameters(params): Parameters<MigrationSafetyParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "migration_safety",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_migration_safety::tool_migration_safety(self.ctx(), params),
        )
        .await
    }
    #[tool(description = "Columns declared in SQL DDL but never referenced anywhere in source.")]
    async fn dead_columns(
        &self,
        Parameters(params): Parameters<DeadColumnsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "dead_columns",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_dead_columns::tool_dead_columns(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "PII detection: PII-shaped literals + PII-named identifiers co-located with logging or network sinks."
    )]
    async fn pii_spread(
        &self,
        Parameters(params): Parameters<PiiSpreadParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "pii_spread",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_pii_spread::tool_pii_spread(self.ctx(), params),
        )
        .await
    }
}
