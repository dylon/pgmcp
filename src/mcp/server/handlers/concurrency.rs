//! SOTA concurrency / safety / performance handlers (part B).
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

#[rmcp::tool_router(router = router_concurrency, vis = "pub(crate)")]
impl McpServer {
    #[tool(
        description = "Lock-order cycles (Havender 1968) by scanning function bodies for lock(A);lock(B) sequences and computing SCCs."
    )]
    async fn deadlock_candidates(
        &self,
        Parameters(params): Parameters<DeadlockCandidatesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "deadlock_candidates",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_deadlock_candidates::tool_deadlock_candidates(
                self.ctx(),
                params,
            ),
        )
        .await
    }
    #[tool(
        description = "Rust Send/Sync violation candidates: Arc<RefCell>, static mut, unsafe Send/Sync impls."
    )]
    async fn send_sync_violations(
        &self,
        Parameters(params): Parameters<SendSyncViolationsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "send_sync_violations",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_send_sync_violations::tool_send_sync_violations(
                self.ctx(),
                params,
            ),
        )
        .await
    }
    #[tool(
        description = "Accidentally-quadratic loops (Petrashko ICSE 2017): for/while loops with .contains/.find/.indexOf in the body."
    )]
    async fn quadratic_loops(
        &self,
        Parameters(params): Parameters<QuadraticLoopsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "quadratic_loops",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_quadratic_loops::tool_quadratic_loops(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Missing-preallocation hotspots: Vec::new/HashMap::new without with_capacity."
    )]
    async fn missing_preallocation(
        &self,
        Parameters(params): Parameters<MissingPreallocationParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "missing_preallocation",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_missing_preallocation::tool_missing_preallocation(
                self.ctx(),
                params,
            ),
        )
        .await
    }
    #[tool(
        description = "Blocking calls inside async fn bodies (std::fs / std::sync::Mutex / time.sleep). \
Tokio anti-pattern: blocks the executor."
    )]
    async fn blocking_in_async(
        &self,
        Parameters(params): Parameters<BlockingInAsyncParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "blocking_in_async",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_blocking_in_async::tool_blocking_in_async(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = ".clone() / Arc::clone density per file × PageRank — surfaces allocation hotspots before profiling."
    )]
    async fn clone_density(
        &self,
        Parameters(params): Parameters<CloneDensityParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "clone_density",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_clone_density::tool_clone_density(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "I/O calls weighted by PageRank + betweenness — finds blocking I/O on hot paths."
    )]
    async fn io_hotpath(
        &self,
        Parameters(params): Parameters<IoHotpathParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "io_hotpath",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_io_hotpath::tool_io_hotpath(self.ctx(), params),
        )
        .await
    }
}
