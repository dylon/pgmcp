//! SOTA call-graph-downstream & evolution-analytics handlers.
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

#[rmcp::tool_router(router = router_callgraph_evo, vis = "pub(crate)")]
impl McpServer {
    #[tool(
        description = "Forward reachability dead-code: symbols unreached from main / public exports / test entry points."
    )]
    async fn dead_code_reachability(
        &self,
        Parameters(params): Parameters<DeadCodeReachabilityParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "dead_code_reachability",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_dead_code_reachability::tool_dead_code_reachability(
                self.ctx(),
                params,
            ),
        )
        .await
    }
    #[tool(
        description = "Feature envy (Lanza-Marinescu 2006): functions whose external-data references dominate own-file references."
    )]
    async fn feature_envy(
        &self,
        Parameters(params): Parameters<FeatureEnvyParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "feature_envy",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_feature_envy::tool_feature_envy(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Shotgun-surgery detection from git history: commits touching many files indicate scattered responsibility."
    )]
    async fn shotgun_surgery(
        &self,
        Parameters(params): Parameters<ShotgunSurgeryParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "shotgun_surgery",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_shotgun_surgery::tool_shotgun_surgery(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "LCOM4 (Hitz-Montazeri 1995): per-container connected components in the member-method shared-target graph."
    )]
    async fn lcom4(
        &self,
        Parameters(params): Parameters<Lcom4Params>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "lcom4",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_lcom4::tool_lcom4(self.ctx(), params),
        )
        .await
    }

    // ========================================================================
    // SOTA Phase 11 — Evolution analytics
    // ========================================================================
    #[tool(
        description = "Refactor pressure (Tufano ICSE 2015): per-file ratio of non-test commits to test commits in the window."
    )]
    async fn refactor_pressure(
        &self,
        Parameters(params): Parameters<RefactorPressureParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "refactor_pressure",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_refactor_pressure::tool_refactor_pressure(self.ctx(), params),
        )
        .await
    }
    #[tool(description = "Page CUSUM change-points on per-file commit rate (weekly).")]
    async fn commit_changepoint(
        &self,
        Parameters(params): Parameters<CommitChangepointParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "commit_changepoint",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_commit_changepoint::tool_commit_changepoint(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "Per-file commit-message vocabulary drift via Porter-stemmed TF cosine across sliding windows."
    )]
    async fn commit_topic_drift(
        &self,
        Parameters(params): Parameters<CommitTopicDriftParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "commit_topic_drift",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_commit_topic_drift::tool_commit_topic_drift(self.ctx(), params),
        )
        .await
    }
    #[tool(
        description = "API stability scored over release-marker commits (Bogart EMSE 2016 adapted)."
    )]
    async fn release_api_stability(
        &self,
        Parameters(params): Parameters<ReleaseApiStabilityParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        instrumented_tool_wrap(
            self.stats(),
            "release_api_stability",
            30,
            &_ctx,
            &summarize_debug(&params),
            crate::mcp::tools::tool_release_api_stability::tool_release_api_stability(
                self.ctx(),
                params,
            ),
        )
        .await
    }
}
