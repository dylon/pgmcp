//! Developer-tool ("toolbox") catalog handlers.
//!
//! `#[tool]` methods for the installed formal-verification and
//! profiling/benchmarking/debugging tools, forwarding to
//! `crate::mcp::tools::tool_toolbox`. The per-block router is composed in
//! `server.rs` via `assembled_tool_router()`.
#![allow(clippy::too_many_lines)]

use crate::mcp::server::McpServer;
use crate::mcp::server::*;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer};

#[rmcp::tool_router(router = router_toolbox, vis = "pub(crate)")]
impl McpServer {
    #[tool(
        description = "Semantic + filterable search over the catalog of formal-verification and \
profiling/benchmarking/debugging tools installed on this machine. \
USE WHEN: you need to pick a tool for a task ('prove a rewrite system terminates', 'find where \
threads block', 'profile heap growth') and want ranked tool cards (what it does, when to use it, \
how to invoke it here) filterable by domain/category. \
DO NOT USE WHEN: searching indexed source files — use semantic_search/hybrid_search for code."
    )]
    async fn toolbox_search(
        &self,
        Parameters(params): Parameters<ToolboxSearchParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "toolbox_search",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_toolbox::tool_toolbox_search(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "Recommend installed tools for a task, ranked. \
USE WHEN: planning an approach and you want the best installed verifier/profiler/debugger for the \
job (e.g. 'verify Rust panic-freedom', 'diagnose lock contention'); domain is inferred from the \
task or can be hinted (formal_verification | developer_tooling).")]
    async fn toolbox_recommend(
        &self,
        Parameters(params): Parameters<ToolboxRecommendParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "toolbox_recommend",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_toolbox::tool_toolbox_recommend(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Fetch one tool card by slug or id (e.g. 'z3', 'valgrind-massif'), with full \
fields: what it does, when to use, inputs/outputs, invocation grounded on this machine, strengths, \
limitations, availability, and cross-linked alternatives."
    )]
    async fn toolbox_get(
        &self,
        Parameters(params): Parameters<ToolboxGetParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "toolbox_get",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_toolbox::tool_toolbox_get(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Browse the toolbox catalog by domain (formal_verification | \
developer_tooling) and/or category (e.g. smt_solver, model_checker, cpu_profiler, ebpf_tracer)."
    )]
    async fn toolbox_list(
        &self,
        Parameters(params): Parameters<ToolboxListParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "toolbox_list",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_toolbox::tool_toolbox_list(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Toolbox catalog statistics: total tools, per-domain and per-category counts, \
and the number of cards still missing embeddings."
    )]
    async fn toolbox_stats(
        &self,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "toolbox_stats",
            30,
            &_ctx,
            "",
            crate::mcp::tools::tool_toolbox::tool_toolbox_stats(self.ctx()),
        )
        .await
    }

    #[tool(
        description = "Admin tool to re-seed the toolbox catalog from the bundled cards. \
mode=seed_only re-upserts cards (the embedding cron re-embeds changed rows); mode=reembed also \
synchronously embeds any NULL-embedding cards for immediate availability. dry_run reports counts."
    )]
    async fn toolbox_refresh(
        &self,
        Parameters(params): Parameters<ToolboxRefreshParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        // mode=reembed embeds ~100 compact cards in-process; 300 s is ample.
        instrumented_tool_wrap(
            self.stats(),
            "toolbox_refresh",
            300,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_toolbox::tool_toolbox_refresh(self.ctx(), params),
        )
        .await
    }
}
