//! Native formal-verification (FV) tool handlers (Task #22 §4-A).
//!
//! Each tool runs entirely in-process — pgmcp/CSM data + `lling_llang::symbolic`
//! engines → verdict — with **no subprocess and no prattail dependency**. The router
//! `router_fv` is composed into `assembled_tool_router()` in `server.rs`.

use crate::mcp::server::McpServer;
use crate::mcp::server::*;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer};

#[rmcp::tool_router(router = router_fv, vis = "pub(crate)")]
impl McpServer {
    #[tool(
        description = "Protocol soundness — deadlock-freedom + progress for a CSM \
            GlobalType, decided IN-PROCESS via MPST well-formedness. Rocq-backed by \
            CsmDeadlockFreedom.v (well_formed ⇒ deadlock-free ∧ progress by typing, \
            Caires–Pfenning/Wadler) — closes the ADR-012 gap that otherwise needs \
            pi+TLC. No subprocess, no prattail. Param: global_type (adjacent-tagged JSON)."
    )]
    async fn protocol_soundness(
        &self,
        Parameters(params): Parameters<ProtocolSoundnessParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "protocol_soundness",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_protocol_soundness::tool_protocol_soundness(
                self.ctx(),
                params,
            ),
        )
        .await
    }
}
