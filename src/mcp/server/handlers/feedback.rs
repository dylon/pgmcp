//! Agent-feedback + voting tool handlers (ADR-023).
//!
//! Thin `#[tool]` forwards into `crate::mcp::tools::tool_feedback::*`. The
//! submit/respond/promote/cast/retract tools inject the caller identity into
//! `agent_id` when the client omits it (same idiom as the work-item tools), so
//! "one vote per (target, agent)" and feedback attribution key on the MCP
//! caller's declared `clientInfo.name`.
#![allow(clippy::too_many_lines)]

use crate::mcp::server::McpServer;
use crate::mcp::server::*;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer};

#[rmcp::tool_router(router = router_feedback, vis = "pub(crate)")]
impl McpServer {
    #[tool(
        description = "Submit feedback about pgmcp itself — what you like, dislike, or want \
feature-wise. Requires `category` (complaint|feature_request|praise|bug_report|question|suggestion) \
and `sentiment` (strongly_negative|negative|neutral|positive|strongly_positive) plus a `body`; \
optional `subject`, `about_tool`, `project`. USE WHEN you want to report a pgmcp pain point, request \
a feature, or praise something. Returns {id, category, sentiment, status}."
    )]
    async fn submit_feedback(
        &self,
        Parameters(mut params): Parameters<SubmitFeedbackParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if params.agent_id.is_none() {
            params.agent_id = Some(extract_caller(&_ctx).client_name);
        }
        instrumented_tool_wrap(
            self.stats(),
            "submit_feedback",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_feedback::tool_submit_feedback(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "List agent feedback, newest first, with optional filters (category, \
sentiment, status, about_tool, project). Returns {count, feedback[]}."
    )]
    async fn list_feedback(
        &self,
        Parameters(params): Parameters<ListFeedbackParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "list_feedback",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_feedback::tool_list_feedback(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Search agent feedback by keyword (fts), meaning (semantic), or both \
(hybrid, default). Returns {mode, count, feedback[]}."
    )]
    async fn search_feedback(
        &self,
        Parameters(params): Parameters<SearchFeedbackParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "search_feedback",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_feedback::tool_search_feedback(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Triage a feedback item: set its status (acknowledged|planned|resolved|\
declined|open) and optionally record a response. Returns {id, status, updated}."
    )]
    async fn respond_feedback(
        &self,
        Parameters(mut params): Parameters<RespondFeedbackParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if params.agent_id.is_none() {
            params.agent_id = Some(extract_caller(&_ctx).client_name);
        }
        instrumented_tool_wrap(
            self.stats(),
            "respond_feedback",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_feedback::tool_respond_feedback(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Promote a feedback item into a tracked work-item (kind=task), linking the \
two. Idempotent. Returns {feedback_id, work_item_public_id, work_item_id, already_promoted}."
    )]
    async fn promote_feedback_to_work_item(
        &self,
        Parameters(mut params): Parameters<PromoteFeedbackParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if params.agent_id.is_none() {
            params.agent_id = Some(extract_caller(&_ctx).client_name);
        }
        instrumented_tool_wrap(
            self.stats(),
            "promote_feedback_to_work_item",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_feedback::tool_promote_feedback_to_work_item(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Cast (or update) a vote on an issue/feedback/work-item/experiment. \
`target_type` ∈ work_item|feedback|bug|experiment, `direction` ∈ up|down, optional `weight`. At most \
one vote per (target, agent); re-voting updates it. Returns {vote_id, tally}."
    )]
    async fn cast_vote(
        &self,
        Parameters(mut params): Parameters<CastVoteParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if params.agent_id.is_none() {
            params.agent_id = Some(extract_caller(&_ctx).client_name);
        }
        instrumented_tool_wrap(
            self.stats(),
            "cast_vote",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_feedback::tool_cast_vote(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Retract your vote on a target. Returns {target_type, target_id, removed}."
    )]
    async fn retract_vote(
        &self,
        Parameters(mut params): Parameters<RetractVoteParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if params.agent_id.is_none() {
            params.agent_id = Some(extract_caller(&_ctx).client_name);
        }
        instrumented_tool_wrap(
            self.stats(),
            "retract_vote",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_feedback::tool_retract_vote(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Tally votes on a target: {up_votes, down_votes, net_weight, voters}. Use to \
rank feedback/work-items by support."
    )]
    async fn tally_votes(
        &self,
        Parameters(params): Parameters<TallyVotesParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "tally_votes",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_feedback::tool_tally_votes(self.ctx(), params),
        )
        .await
    }
}
