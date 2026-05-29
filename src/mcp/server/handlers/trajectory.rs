//! Trajectory-similarity & recursive-pattern handlers.
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

#[rmcp::tool_router(router = router_trajectory, vis = "pub(crate)")]
impl McpServer {
    #[tool(
        description = "Recursive Language Model decomposition (Part B): treat a corpus/file as an external \
environment, decompose into snippets, recursively sub-call a peer LM over each (small context), and stitch \
the partials — the full context is never inlined. Solves beyond-context-window queries over indexed code."
    )]
    async fn a2a_pattern_recursive(
        &self,
        Parameters(params): Parameters<A2aPatternRecursiveParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "a2a_pattern_recursive",
            600,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_a2a_pattern_recursive::tool_a2a_pattern_recursive(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "MSM trajectory similarity (Part B): retrieve the most similar past RLM runs to a probe \
(Move-Split-Merge distance over their step sequences) and classify whether it trends toward success or \
failure. Powers the 'learn which decomposition worked' loop."
    )]
    async fn trajectory_similarity(
        &self,
        Parameters(params): Parameters<TrajectorySimilarityParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "trajectory_similarity",
            60,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_trajectory_similarity::tool_trajectory_similarity(
                self.ctx(),
                params,
            ),
        )
        .await
    }

    #[tool(
        description = "Stage 5d online recognition: match a partial / in-progress numeric \
trajectory ('work_item' progress-% or 'file' weekly-churn) against the live record cohort via \
Move-Split-Merge (which aligns different-length sequences), returning the nearest known \
trajectories. Feed an unfolding series for early-warning / outcome prediction."
    )]
    async fn recognize_trajectory(
        &self,
        Parameters(params): Parameters<RecognizeTrajectoryParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "recognize_trajectory",
            60,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_trajectory_similarity::tool_recognize_trajectory(
                self.ctx(),
                params,
            ),
        )
        .await
    }
}
