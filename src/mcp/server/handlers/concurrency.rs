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
        description = "Interprocedural lock-order deadlock cycles with lock identity from the \
            sync_ops skeleton: per-symbol held-set + callee-lock inlining across the resolved \
            call graph (RacerD-style), SCC cycles with witnessing call sites + severity. \
            Supersedes the shallow intra-function deadlock_candidates; TLA+/Rocq-backed (ADR-011)."
    )]
    async fn deadlock_cycles(
        &self,
        Parameters(params): Parameters<DeadlockCyclesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "deadlock_cycles",
            60,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_deadlock_cycles::tool_deadlock_cycles(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Inspect the interprocedural lock-order graph (nodes = lock resources, \
            edges = \"B acquired while A held\", cyclic SCCs); optional resource_key neighborhood."
    )]
    async fn lock_order_graph(
        &self,
        Parameters(params): Parameters<LockOrderGraphParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "lock_order_graph",
            60,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_lock_order_graph::tool_lock_order_graph(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Reconcile a RUNTIME trace (BCC off-CPU folded stacks, `perf script`, or a \
            `gdb thread apply all bt`) against the STATIC interprocedural lock-order graph. Returns \
            confirmed waits (static predicted them), static_missed (real runtime waits the static \
            graph lacks — precision gaps), and static_only (static edges this trace didn't exercise), \
            plus static cycles flagged when runtime-corroborated. Read-only — pgmcp parses the \
            agent-provided trace text; it never attaches a debugger. format = offcpu_folded | \
            perf_script | gdb_bt."
    )]
    async fn runtime_deadlock_reconcile(
        &self,
        Parameters(params): Parameters<RuntimeDeadlockReconcileParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "runtime_deadlock_reconcile",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_runtime_deadlock_reconcile::tool_runtime_deadlock_reconcile(
                self.ctx(),
                params,
            ),
        )
        .await
    }
    #[tool(
        description = "Map an agent-provided backtrace (gdb `bt`, a BCC off-CPU folded stack, or a \
            newline/`;`-separated frame list) to file:line + symbol per frame, with the memory-graph \
            entities anchored to each resolved symbol. Turns an opaque trace into clickable code \
            locations enriched with prior knowledge. Read-only (symbol resolution + memory anchors). \
            format = gdb_bt | folded | auto."
    )]
    async fn trace_map_to_code(
        &self,
        Parameters(params): Parameters<TraceMapToCodeParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "trace_map_to_code",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_trace_map_to_code::tool_trace_map_to_code(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Inspect one symbol's ordered synchronization skeleton (sync_ops) with a \
            per-op held-set annotation (drill-down for a reported lock cycle)."
    )]
    async fn sync_skeleton(
        &self,
        Parameters(params): Parameters<SyncSkeletonParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "sync_skeleton",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_sync_skeleton::tool_sync_skeleton(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Message-passing (channel) deadlock signals over the sync_ops skeleton: \
            blocked_recv (linear receive with no producer), orphan_send (send never received), \
            channel_cycle (processes mutually blocked on each other's sends). Petri-net \
            structural analysis; TLA+/Rocq-backed (ADR-011)."
    )]
    async fn channel_deadlock(
        &self,
        Parameters(params): Parameters<ChannelDeadlockParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "channel_deadlock",
            60,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_channel_deadlock::tool_channel_deadlock(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Concurrency choke-point ranking over sync_ops, weighted by file pagerank: \
            lock contention (one lock acquired by many symbols), channel imbalance (send/recv \
            skew), spawn fan-out, and async stalls (await + blocking I/O). Complements io_hotpath."
    )]
    async fn concurrency_bottlenecks(
        &self,
        Parameters(params): Parameters<ConcurrencyBottlenecksParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "concurrency_bottlenecks",
            60,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_concurrency_bottlenecks::tool_concurrency_bottlenecks(
                self.ctx(),
                params,
            ),
        )
        .await
    }
    #[tool(
        description = "Forecast a concurrency-health metric (deadlock_cycle_count / \
            max_lock_contention / …) over concurrency_health_history via OLS: current value, \
            slope/day, % change, and weeks-to-threshold ETA. Mirrors quality_forecast (ADR-011)."
    )]
    async fn concurrency_forecast(
        &self,
        Parameters(params): Parameters<ConcurrencyForecastParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "concurrency_forecast",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_concurrency_forecast::tool_concurrency_forecast(
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
