//! `crucible_trace_*` tool handlers — unified run tracing (ADR-020 E10).
//!
//! The per-block router (`router_trace`) is composed in `server.rs` via
//! `assembled_tool_router()`. Bodies live in
//! `crate::mcp::tools::tool_crucible_trace`; these are the thin MCP `#[tool]`
//! wrappers (the same shape as `handlers/csm.rs`). Record tools write pgmcp's own
//! trace tables; query/replay tools are pure reads — the no-file boundary holds.
#![allow(clippy::too_many_lines)]

use crate::mcp::server::McpServer;
use crate::mcp::server::*;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer};

#[rmcp::tool_router(router = router_trace, vis = "pub(crate)")]
impl McpServer {
    #[tool(
        description = "ADR-020: open a trace span (status unset, no end) for a long-running step. \
Returns {span_id, trace_id}. Pair with crucible_trace_close_span."
    )]
    async fn crucible_trace_open_span(
        &self,
        Parameters(params): Parameters<CrucibleTraceSpanParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "crucible_trace_open_span",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_crucible_trace::tool_crucible_trace_open_span(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "ADR-020: record a complete span in one shot (the per-step path the crucible-trace \
extension uses). Carries model/peer/cursor/digests + status. Returns {span_id, trace_id}."
    )]
    async fn crucible_trace_record_span(
        &self,
        Parameters(params): Parameters<CrucibleTraceSpanParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "crucible_trace_record_span",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_crucible_trace::tool_crucible_trace_record_span(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(description = "ADR-020: close an open span (set terminal status + ended_at).")]
    async fn crucible_trace_close_span(
        &self,
        Parameters(params): Parameters<CrucibleTraceCloseParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "crucible_trace_close_span",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_crucible_trace::tool_crucible_trace_close_span(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "ADR-020: append a point-in-time annotation to a span (model_chosen|retry|failure|\
counterexample_found|critic_verdict|halt|resume|...)."
    )]
    async fn crucible_trace_event(
        &self,
        Parameters(params): Parameters<CrucibleTraceEventParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "crucible_trace_event",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_crucible_trace::tool_crucible_trace_event(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "ADR-020/D3: persist a TLC/SMT/Rocq counterexample as a structured, replayable \
witness (idempotent on content_sha256). The ephemeral-stdout->durable-artifact step."
    )]
    async fn crucible_trace_record_counterexample(
        &self,
        Parameters(params): Parameters<CrucibleRecordCexParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "crucible_trace_record_counterexample",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_crucible_trace::tool_crucible_trace_record_counterexample(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "ADR-016/ADR-020/D4: append a control-plane action (halt|resume|cancel|checkpoint|\
fork|...) to the append-only audit journal."
    )]
    async fn crucible_trace_control(
        &self,
        Parameters(params): Parameters<CrucibleControlParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "crucible_trace_control",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_crucible_trace::tool_crucible_trace_control(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "ADR-020: a run's header + summary counts (by trace_id or session_key).")]
    async fn crucible_trace_get(
        &self,
        Parameters(params): Parameters<CrucibleTraceRefParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "crucible_trace_get",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_crucible_trace::tool_crucible_trace_get(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "ADR-020: the ordered span timeline (+ annotations) of a run.")]
    async fn crucible_trace_timeline(
        &self,
        Parameters(params): Parameters<CrucibleTraceTimelineParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "crucible_trace_timeline",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_crucible_trace::tool_crucible_trace_timeline(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "ADR-020: cross-trace span filter (kind|status|role|model|work_item|experiment|time)."
    )]
    async fn crucible_trace_query(
        &self,
        Parameters(params): Parameters<CrucibleTraceQueryParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "crucible_trace_query",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_crucible_trace::tool_crucible_trace_query(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "ADR-020/ADR-011: step-debug a recorded run — replay a prefix (to_event/to_step) and \
recover per-role LocalState + the orchestrator's next move. Refuses loudly on an off-protocol prefix."
    )]
    async fn crucible_trace_replay(
        &self,
        Parameters(params): Parameters<CrucibleTraceReplayParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "crucible_trace_replay",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_crucible_trace::tool_crucible_trace_replay(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "ADR-020: reconcile the observed run against the planned GlobalType → the first \
divergence (off-protocol step | stall | unbalanced), or conformant. The runtime_deadlock_reconcile analogue."
    )]
    async fn crucible_trace_reconcile(
        &self,
        Parameters(params): Parameters<CrucibleTraceRefParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "crucible_trace_reconcile",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_crucible_trace::tool_crucible_trace_reconcile(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "ADR-020: walk to the first failure + its cause — the first error span, the \
protocol first-divergence, and the replayed position there."
    )]
    async fn crucible_trace_why(
        &self,
        Parameters(params): Parameters<CrucibleTraceRefParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "crucible_trace_why",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_crucible_trace::tool_crucible_trace_why(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "ADR-020: structural diff of a failing vs passing run, aligned on (cursor, kind, \
role) → the first divergence (the regression 'what changed')."
    )]
    async fn crucible_trace_diff(
        &self,
        Parameters(params): Parameters<CrucibleTraceDiffParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "crucible_trace_diff",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_crucible_trace::tool_crucible_trace_diff(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "ADR-016/ADR-020/D4: the control-plane audit history (halt/resume/cancel/checkpoint \
with channel + reason + affected sessions/tasks)."
    )]
    async fn crucible_trace_audit(
        &self,
        Parameters(params): Parameters<CrucibleTraceAuditParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "crucible_trace_audit",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_crucible_trace::tool_crucible_trace_audit(self.ctx(), params),
        )
        .await
    }

    #[tool(description = "ADR-020/D3: fetch a persisted counterexample witness (by id, or latest for a trace).")]
    async fn crucible_trace_counterexample(
        &self,
        Parameters(params): Parameters<CrucibleTraceCexParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "crucible_trace_counterexample",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_crucible_trace::tool_crucible_trace_counterexample(
                self.ctx(),
                params,
            ),
        )
        .await
    }
}
