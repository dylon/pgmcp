//! CSM / MPST coordination observer tool handlers (ADR-009).
//!
//! Tool handlers extracted verbatim from `server.rs` (B.3 god-file split).
//! Only the relative `super::tools::` path was rewritten to the absolute
//! `crate::mcp::tools::` (the module is unchanged); bodies are otherwise
//! identical. The per-block router is composed in `server.rs` via
//! `assembled_tool_router()`.
#![allow(clippy::too_many_lines)]

use crate::mcp::server::McpServer;
use crate::mcp::server::*;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer};

#[rmcp::tool_router(router = router_csm, vis = "pub(crate)")]
impl McpServer {
    #[tool(
        description = "List the CSM/MPST coordination protocols (the five RecursiveMAS patterns) \
with participants and well-formedness. ADR-009."
    )]
    async fn csm_list_protocols(
        &self,
        Parameters(params): Parameters<CsmListProtocolsParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "csm_list_protocols",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_csm_list_protocols::tool_csm_list_protocols(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Show one coordination pattern's global type (the MPST AST), participants, \
and well-formedness. ADR-009."
    )]
    async fn csm_protocol_of_pattern(
        &self,
        Parameters(params): Parameters<CsmProtocolOfPatternParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "csm_protocol_of_pattern",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_csm_protocol_of_pattern::tool_csm_protocol_of_pattern(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Show the per-role local machines a coordination pattern projects to \
(G ↾ role); a role that does not project surfaces its projection error. ADR-009."
    )]
    async fn csm_show_projection(
        &self,
        Parameters(params): Parameters<CsmShowProjectionParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "csm_show_projection",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_csm_show_projection::tool_csm_show_projection(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Validate a completed a2a_pattern_* run against its coordination protocol: \
lift the recorded transcript into a trace, check conformance, and persist the verdict to \
csm_run_traces. ADR-009."
    )]
    async fn csm_validate_run(
        &self,
        Parameters(params): Parameters<CsmValidateRunParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "csm_validate_run",
            60,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_csm_validate_run::tool_csm_validate_run(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Show the protocol interpreter's prescribed orchestrator communication order \
for a pattern (the ProtocolDriver plan). Linear patterns (sequential/mixture/distillation/recursive) \
are drivable; Deliberation is not (runtime choice). ADR-009 Phase 6."
    )]
    async fn csm_protocol_plan(
        &self,
        Parameters(params): Parameters<CsmProtocolPlanParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "csm_protocol_plan",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_csm_protocol_plan::tool_csm_protocol_plan(self.ctx(), params),
        )
        .await
    }

    #[tool(
        description = "Infer a peer's behaviour FSM from a protocol's accumulated run traces \
(passive prefix-tree automaton with observation counts) and diff it against the declared protocol — \
novel symbols flag off-protocol behaviour. ADR-009 Phase 8."
    )]
    async fn csm_infer_peer_fsm(
        &self,
        Parameters(params): Parameters<CsmInferPeerFsmParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "csm_infer_peer_fsm",
            60,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_csm_infer_peer_fsm::tool_csm_infer_peer_fsm(self.ctx(), params),
        )
        .await
    }
}
