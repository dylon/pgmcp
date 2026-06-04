//! A2A inter-agent IPC & RecursiveMAS pattern handlers.
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

#[rmcp::tool_router(router = router_a2a, vis = "pub(crate)")]
impl McpServer {
    #[tool(
        description = "Dispatch a Task to a registered A2A peer agent. Returns the final Task with status and artifacts."
    )]
    async fn a2a_send_task(
        &self,
        Parameters(params): Parameters<A2aSendTaskParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "a2a_send_task",
            60,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_a2a_send_task::tool_a2a_send_task(self.ctx(), params),
        )
        .await
    }
    #[tool(description = "Poll a Task on a registered A2A peer agent.")]
    async fn a2a_get_task(
        &self,
        Parameters(params): Parameters<A2aGetTaskParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "a2a_get_task",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_a2a_get_task::tool_a2a_get_task(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "List currently-active agent instances and the project each is working on, grouped by project — with PID, liveness, and (if registered) A2A role/specialty. \
USE WHEN: you want to discover which agents are live on a project of interest (e.g. before messaging one that is editing a dependency you build on). \
The returned `mcp_session_id` is the precise instance handle to address with `a2a_send_message`. `project` filters to one project by name."
    )]
    async fn a2a_active_agents(
        &self,
        Parameters(params): Parameters<A2aActiveAgentsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "a2a_active_agents",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_a2a_active_agents::tool_a2a_active_agents(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Send a message into a peer agent's mailbox — addressable by `to_session` (a precise live instance: the `mcp_session_id` from a2a_active_agents), `to_project` (any agent working there), or `to_agent` (a client type). Complements `a2a_send_task` (which spawns a new agent) with a mailbox to LIVE instances. `kind` defaults to 'message'. The sender (from_agent/from_session) is auto-filled with your identity."
    )]
    async fn a2a_send_message(
        &self,
        Parameters(mut params): Parameters<A2aSendMessageParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if params.from_agent.is_none() {
            params.from_agent = Some(extract_caller(&_ctx).client_name);
        }
        if params.from_session.is_none() {
            params.from_session = extract_mcp_session_id(&_ctx);
        }
        instrumented_tool_wrap(
            self.stats(),
            "a2a_send_message",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_a2a_send_message::tool_a2a_send_message(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Read messages addressed to you — the reliable inbox pull. With no args it returns messages for your own session; pass `project` to also see project-addressed messages, or `agent` for client-type broadcasts. Reading marks them read."
    )]
    async fn a2a_inbox(
        &self,
        Parameters(mut params): Parameters<A2aInboxParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if params.session.is_none() && params.project.is_none() && params.agent.is_none() {
            params.session = extract_mcp_session_id(&_ctx);
        }
        instrumented_tool_wrap(
            self.stats(),
            "a2a_inbox",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_a2a_inbox::tool_a2a_inbox(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Reply to a mailbox message; the reply is addressed back to the original sender and linked via reply_to."
    )]
    async fn a2a_reply_message(
        &self,
        Parameters(mut params): Parameters<A2aReplyMessageParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if params.from_agent.is_none() {
            params.from_agent = Some(extract_caller(&_ctx).client_name);
        }
        if params.from_session.is_none() {
            params.from_session = extract_mcp_session_id(&_ctx);
        }
        instrumented_tool_wrap(
            self.stats(),
            "a2a_reply_message",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_a2a_reply_message::tool_a2a_reply_message(self.ctx(), params),
        )
        .await
    }
    #[tool(description = "Acknowledge a mailbox message (stamps acked_at for your session).")]
    async fn a2a_ack_message(
        &self,
        Parameters(mut params): Parameters<A2aAckMessageParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if params.session.is_none() {
            params.session = extract_mcp_session_id(&_ctx);
        }
        instrumented_tool_wrap(
            self.stats(),
            "a2a_ack_message",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_a2a_ack_message::tool_a2a_ack_message(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Return the SSE URL for streaming events from a peer's Task. Caller opens the URL with Accept: text/event-stream."
    )]
    async fn a2a_subscribe_task(
        &self,
        Parameters(params): Parameters<A2aSubscribeTaskParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "a2a_subscribe_task",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_a2a_subscribe_task::tool_a2a_subscribe_task(self.ctx(), params),
        )
        .await
    }
    #[tool(description = "Cancel a Task on a registered A2A peer agent.")]
    async fn a2a_cancel_task(
        &self,
        Parameters(params): Parameters<A2aCancelTaskParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "a2a_cancel_task",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_a2a_cancel_task::tool_a2a_cancel_task(self.ctx(), params),
        )
        .await
    }
    #[tool(description = "Register a peer A2A agent in the local directory. Upserts by name.")]
    async fn a2a_register_agent(
        &self,
        Parameters(params): Parameters<A2aRegisterAgentParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "a2a_register_agent",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_a2a_register_agent::tool_a2a_register_agent(self.ctx(), params),
        )
        .await
    }
    #[tool(description = "List all registered A2A peer agents in the local directory.")]
    async fn a2a_list_agents(
        &self,
        Parameters(params): Parameters<A2aListAgentsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "a2a_list_agents",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_a2a_list_agents::tool_a2a_list_agents(self.ctx(), params),
        )
        .await
    }

    // ========================================================================
    // A2A RecursiveMAS-inspired extensions (Yang et al. 2026 Table 1)
    // ========================================================================

    #[tool(
        description = "Find registered A2A peers matching specialty tags / role. \
Useful before invoking a collaboration pattern so you can pick the right peer for each role."
    )]
    async fn a2a_find_agents_by_specialty(
        &self,
        Parameters(params): Parameters<A2aFindAgentsBySpecialtyParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "a2a_find_agents_by_specialty",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_a2a_find_agents_by_specialty::tool_a2a_find_agents_by_specialty(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Use WHEN work has clear ordered stages and each stage should critique the \
last: Planner → Critic → Solver, each peer's output conditioning the next. DO NOT USE for \
independent parallel subtasks — use `a2a_pattern_mixture`. Returns the run with an inline protocol \
verdict; feed the learner afterward with `csm_validate_run(task_id)`. (RecursiveMAS Table 1 Sequential.)"
    )]
    async fn a2a_pattern_sequential(
        &self,
        Parameters(params): Parameters<A2aPatternSequentialParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "a2a_pattern_sequential",
            300,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_a2a_pattern_sequential::tool_a2a_pattern_sequential(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Use WHEN you want breadth: fan the same question out to N specialist peers in \
parallel, then a Summarizer aggregates their takes. DO NOT USE when each step depends on the \
previous — use `a2a_pattern_sequential`. (RecursiveMAS Table 1 Mixture.)"
    )]
    async fn a2a_pattern_mixture(
        &self,
        Parameters(params): Parameters<A2aPatternMixtureParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "a2a_pattern_mixture",
            300,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_a2a_pattern_mixture::tool_a2a_pattern_mixture(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Use WHEN you have a thorough answer and want a compact, teachable rationale: \
Expert → Learner, returning both for a latency/quality comparison (e.g. to write a durable note or \
doc). DO NOT USE for multi-stage problem solving — use `a2a_pattern_sequential`. (RecursiveMAS \
Table 1 Distillation.)"
    )]
    async fn a2a_pattern_distillation(
        &self,
        Parameters(params): Parameters<A2aPatternDistillationParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "a2a_pattern_distillation",
            300,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_a2a_pattern_distillation::tool_a2a_pattern_distillation(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Use WHEN a problem is hard and benefits from iterate-and-check loops: \
Reflector proposes next sub-tasks, Tool-Caller executes/verifies, repeating until convergence. \
Higher latency — reserve for genuinely hard problems; for breadth use `a2a_pattern_mixture`, for \
ordered stages `a2a_pattern_sequential`. (RecursiveMAS Table 1 Deliberation.)"
    )]
    async fn a2a_pattern_deliberation(
        &self,
        Parameters(params): Parameters<A2aPatternDeliberationParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "a2a_pattern_deliberation",
            300,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_a2a_pattern_deliberation::tool_a2a_pattern_deliberation(
                self.ctx(),
                params,
            ),
        )
        .await
    }
}
